//! Mock UniFi Protect Integration API for end-to-end testing without cameras.
//!
//! `cargo run --example mock_protect -- [--port 7447] [--interval 20]`, then point sauron at
//! `host = "http://127.0.0.1:7447"` with `api_key = "mock"`.
//! `curl -X POST 'http://127.0.0.1:7447/mock/trigger?camera=Driveway&kind=person'` fires an event.

use std::collections::HashSet;
use std::io;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tokio::time::{Instant, interval_at, sleep};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{Callback, ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http;

const API_KEY: &str = "mock";
const API: &str = "/proxy/protect/integration";
const UNAUTHENTICATED: &str =
    r#"{"error":"Failed to authenticate request using 'apiKey'","name":"UNAUTHENTICATED","type":"apiKey"}"#;
const MAX_HEAD: usize = 16 * 1024;

struct CameraDef {
    id: &'static str,
    name: &'static str,
    model: &'static str,
    types: &'static [&'static str],
    hue: u32,
}

const CAMERAS: [CameraDef; 5] = [
    CameraDef {
        id: "66d025b301ebc903e8000001",
        name: "Front Door",
        model: "G4 Doorbell Pro",
        types: &["person", "vehicle", "package", "animal", "face"],
        hue: 0,
    },
    CameraDef {
        id: "66d025b301ebc903e8000002",
        name: "Driveway",
        model: "G5 Bullet",
        types: &["person", "vehicle", "animal", "licensePlate"],
        hue: 72,
    },
    CameraDef {
        id: "66d025b301ebc903e8000003",
        name: "Backyard",
        model: "G4 Instant",
        types: &["person", "animal"],
        hue: 144,
    },
    CameraDef { id: "66d025b301ebc903e8000004", name: "Garage", model: "G3 Flex", types: &[], hue: 216 },
    CameraDef { id: "66d025b301ebc903e8000005", name: "Side Gate", model: "G5 Flex", types: &[], hue: 288 },
];
const FRONT_DOOR: usize = 0;
const SIDE_GATE: usize = 4;

struct State {
    connected: [bool; CAMERAS.len()],
    streams: HashSet<(usize, String)>,
    rng: u64,
}

struct Mock {
    state: Mutex<State>,
    events: broadcast::Sender<String>,
    devices: broadcast::Sender<String>,
    drawtext: bool,
}

impl Mock {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// xorshift64; good enough for picking cameras and ids.
    fn random(&self) -> u64 {
        let mut state = self.state();
        let mut x = state.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        state.rng = x;
        x
    }

    fn pick<T: Copy>(&self, items: &[T]) -> Option<T> {
        (!items.is_empty()).then(|| items[(self.random() % items.len() as u64) as usize])
    }

    fn new_id(&self) -> String {
        format!("{:016x}{:08x}", self.random(), self.random() as u32)
    }

    fn connected(&self, camera: usize) -> bool {
        self.state().connected[camera]
    }

    fn publish_event(&self, frame: Value) {
        eprintln!("mock: event {frame}");
        let _ = self.events.send(frame.to_string());
    }

    fn camera_json(&self, index: usize) -> Value {
        let camera = &CAMERAS[index];
        json!({
            "id": camera.id,
            "modelKey": "camera",
            "state": if self.connected(index) { "CONNECTED" } else { "DISCONNECTED" },
            "name": camera.name,
            "type": camera.model,
            "mac": format!("24A43C3DFE0{index}"),
            "isMicEnabled": true,
            "featureFlags": {
                "supportFullHdSnapshot": true,
                "hasHdr": true,
                "smartDetectTypes": camera.types,
                "smartDetectAudioTypes": [],
                "videoModes": ["default"],
                "hasMic": true,
                "hasLedStatus": true,
                "hasSpeaker": index == FRONT_DOOR,
            },
            "hasPackageCamera": index == FRONT_DOOR,
        })
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> io::Result<()> {
    let mut port: u16 = 7447;
    let mut interval_secs: u64 = 20;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let value = args.next().and_then(|v| v.parse().ok());
        match (arg.as_str(), value) {
            ("--port", Some(v)) => port = u16::try_from(v).map_err(io::Error::other)?,
            ("--interval", Some(v)) if v > 0 => interval_secs = v,
            _ => {
                eprintln!("usage: mock_protect [--port 7447] [--interval 20]");
                std::process::exit(2);
            }
        }
    }

    let mock = Arc::new(Mock {
        state: Mutex::new(State {
            connected: [true, true, true, true, false],
            streams: HashSet::new(),
            rng: now_ms() | 1,
        }),
        events: broadcast::channel(64).0,
        devices: broadcast::channel(64).0,
        drawtext: has_drawtext().await,
    });
    let listener = TcpListener::bind(("127.0.0.1", port)).await?;
    eprintln!(
        "mock: Protect on http://127.0.0.1:{port} (api key \"{API_KEY}\", events every {interval_secs}s, drawtext {})",
        if mock.drawtext { "on" } else { "off" }
    );

    tokio::spawn(generate_events(mock.clone(), Duration::from_secs(interval_secs)));
    tokio::spawn(toggle_side_gate(mock.clone()));
    loop {
        let (stream, _) = listener.accept().await?;
        let mock = mock.clone();
        tokio::spawn(async move {
            if let Err(err) = serve(&mock, stream).await {
                eprintln!("mock: connection error: {err}");
            }
        });
    }
}

async fn has_drawtext() -> bool {
    let output = tokio::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-filters"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .await;
    output.is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains(" drawtext "))
}

// ---- HTTP -----------------------------------------------------------------------------------

struct Head {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
}

impl Head {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    fn authorized(&self) -> bool {
        self.header("x-api-key") == Some(API_KEY)
    }
}

/// Reads the request head; returns it with every byte read so far (head plus any body prefix).
async fn read_head(stream: &mut TcpStream) -> io::Result<(Head, Vec<u8>, usize)> {
    let mut buf = Vec::with_capacity(1024);
    let head_len = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
        if buf.len() > MAX_HEAD {
            return Err(io::Error::other("request head too large"));
        }
        if stream.read_buf(&mut buf).await? == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "connection closed mid-request"));
        }
    };
    let text = std::str::from_utf8(&buf[..head_len]).map_err(io::Error::other)?;
    let mut lines = text.split("\r\n");
    let mut request_line = lines.next().unwrap_or_default().split(' ');
    let (Some(method), Some(target)) = (request_line.next(), request_line.next()) else {
        return Err(io::Error::other("malformed request line"));
    };
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
        .collect();
    let head = Head { method: method.to_owned(), target: target.to_owned(), headers };
    Ok((head, buf, head_len))
}

async fn serve(mock: &Arc<Mock>, mut stream: TcpStream) -> io::Result<()> {
    let (head, mut buf, head_len) = read_head(&mut stream).await?;
    let url = reqwest::Url::parse(&format!("http://mock{}", head.target)).map_err(io::Error::other)?;
    let is_upgrade = head.header("upgrade").is_some_and(|u| u.eq_ignore_ascii_case("websocket"));
    if is_upgrade {
        let topic = match url.path().strip_prefix(API) {
            Some("/v1/subscribe/events") => &mock.events,
            Some("/v1/subscribe/devices") => &mock.devices,
            _ => return respond(&mut stream, 404, "application/json", br#"{"error":"Not found","name":"NOT_FOUND"}"#).await,
        };
        let replay = Rewind { prefix: buf, pos: 0, inner: stream };
        return websocket(replay, topic.subscribe(), url.path().to_owned()).await;
    }

    let length: usize = head.header("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
    while buf.len() < head_len + length {
        if stream.read_buf(&mut buf).await? == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "connection closed mid-body"));
        }
    }
    let body = &buf[head_len..head_len + length];
    let (status, content_type, response) = route(mock, &head, &url, body).await;
    eprintln!("mock: {} {} -> {status}", head.method, head.target);
    respond(&mut stream, status, content_type, &response).await
}

async fn respond(stream: &mut TcpStream, status: u16, content_type: &str, body: &[u8]) -> io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        503 => "Service Unavailable",
        _ => "Internal Server Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.shutdown().await
}

type Reply = (u16, &'static str, Vec<u8>);

fn json_reply(status: u16, value: &Value) -> Reply {
    (status, "application/json", value.to_string().into_bytes())
}

fn not_found() -> Reply {
    json_reply(404, &json!({"error": "Not found", "name": "NOT_FOUND"}))
}

async fn route(mock: &Arc<Mock>, head: &Head, url: &reqwest::Url, body: &[u8]) -> Reply {
    let query = |key: &str| url.query_pairs().find(|(k, _)| k == key).map(|(_, v)| v.into_owned());
    if head.method == "POST" && url.path() == "/mock/trigger" {
        return trigger(mock, query("camera").as_deref(), query("kind").as_deref().unwrap_or("person"));
    }
    let Some(path) = url.path().strip_prefix(API) else { return not_found() };
    if !head.authorized() {
        return (401, "application/json", UNAUTHENTICATED.as_bytes().to_vec());
    }
    let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    match (head.method.as_str(), segments.as_slice()) {
        ("GET", ["v1", "meta", "info"]) => json_reply(200, &json!({"applicationVersion": "6.1.79"})),
        ("GET", ["v1", "cameras"]) => {
            json_reply(200, &Value::Array((0..CAMERAS.len()).map(|i| mock.camera_json(i)).collect()))
        }
        ("GET", ["v1", "cameras", id]) => match camera_index(id) {
            Some(index) => json_reply(200, &mock.camera_json(index)),
            None => not_found(),
        },
        ("GET", ["v1", "cameras", id, "snapshot"]) => {
            let Some(index) = camera_index(id) else { return not_found() };
            if !mock.connected(index) {
                return json_reply(503, &json!({"error": "The camera is offline or not reachable.", "name": "OFFLINE"}));
            }
            match render_snapshot(mock, index, query("highQuality").as_deref() == Some("true")).await {
                Ok(jpeg) => (200, "image/jpeg", jpeg),
                Err(err) => json_reply(500, &json!({"error": err.to_string(), "name": "API_ERROR"})),
            }
        }
        ("GET", ["v1", "cameras", id, "rtsps-stream"]) => {
            let Some(index) = camera_index(id) else { return not_found() };
            let state = mock.state();
            let url = |q: &str| state.streams.contains(&(index, q.to_owned())).then(|| stream_url(index, q));
            json_reply(200, &json!({"high": url("high"), "medium": url("medium"), "low": url("low"), "package": null}))
        }
        ("POST", ["v1", "cameras", id, "rtsps-stream"]) => {
            let Some(index) = camera_index(id) else { return not_found() };
            let qualities: Vec<String> = serde_json::from_slice::<Value>(body)
                .ok()
                .and_then(|v| serde_json::from_value(v["qualities"].clone()).ok())
                .unwrap_or_default();
            if qualities.is_empty() || !qualities.iter().all(|q| matches!(q.as_str(), "high" | "medium" | "low")) {
                return json_reply(400, &json!({"error": "invalid qualities", "name": "BAD_REQUEST"}));
            }
            let mut state = mock.state();
            let mut created = serde_json::Map::new();
            for quality in qualities {
                created.insert(quality.clone(), stream_url(index, &quality).into());
                state.streams.insert((index, quality));
            }
            json_reply(200, &Value::Object(created))
        }
        _ => not_found(),
    }
}

fn camera_index(id_or_name: &str) -> Option<usize> {
    CAMERAS.iter().position(|c| c.id == id_or_name || c.name.eq_ignore_ascii_case(id_or_name))
}

fn stream_url(index: usize, quality: &str) -> String {
    format!("rtsp://127.0.0.1:8554/{}-{quality}?enableSrtp", CAMERAS[index].id)
}

async fn render_snapshot(mock: &Mock, index: usize, high_quality: bool) -> io::Result<Vec<u8>> {
    let camera = &CAMERAS[index];
    let size = if high_quality { "1920x1080" } else { "640x360" };
    let filter = if mock.drawtext {
        format!(
            "drawtext=text='{}  %{{localtime\\:%T}}':fontcolor=white:fontsize=h/10:box=1:boxcolor=black@0.6:boxborderw=8:x=(w-tw)/2:y=(h-th)/2",
            camera.name
        )
    } else {
        format!("hue=h={}", camera.hue)
    };
    let output = tokio::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i"])
        .arg(format!("testsrc2=size={size}:rate=1"))
        .args(["-frames:v", "1", "-vf", &filter, "-c:v", "mjpeg", "-q:v", "5", "-f", "image2pipe", "pipe:1"])
        .stdin(Stdio::null())
        .output()
        .await?;
    if !output.status.success() || output.stdout.is_empty() {
        return Err(io::Error::other(format!("ffmpeg failed: {}", String::from_utf8_lossy(&output.stderr).trim())));
    }
    Ok(output.stdout)
}

// ---- websockets -----------------------------------------------------------------------------

/// Replays the already-read request head before reading from the socket.
struct Rewind {
    prefix: Vec<u8>,
    pos: usize,
    inner: TcpStream,
}

impl AsyncRead for Rewind {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        if self.pos < self.prefix.len() {
            let n = buf.remaining().min(self.prefix.len() - self.pos);
            buf.put_slice(&self.prefix[self.pos..self.pos + n]);
            self.pos += n;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for Rewind {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Handshake callback: rejects upgrades without the API key like the real console.
struct RequireKey;

impl Callback for RequireKey {
    fn on_request(self, request: &Request, response: Response) -> Result<Response, ErrorResponse> {
        if request.headers().get("x-api-key").is_some_and(|k| k == API_KEY) {
            return Ok(response);
        }
        let mut error = ErrorResponse::new(Some(UNAUTHENTICATED.to_owned()));
        *error.status_mut() = http::StatusCode::UNAUTHORIZED;
        error.headers_mut().insert(http::header::CONTENT_TYPE, http::HeaderValue::from_static("application/json"));
        Err(error)
    }
}

async fn websocket(stream: Rewind, mut frames: broadcast::Receiver<String>, path: String) -> io::Result<()> {
    let mut ws = match tokio_tungstenite::accept_hdr_async(stream, RequireKey).await {
        Ok(ws) => ws,
        Err(err) => {
            eprintln!("mock: websocket {path} rejected: {err}");
            return Ok(());
        }
    };
    eprintln!("mock: websocket {path} open");
    loop {
        tokio::select! {
            frame = frames.recv() => match frame {
                Ok(text) => ws.send(Message::text(text)).await.map_err(io::Error::other)?,
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => break,
            },
            incoming = ws.next() => match incoming {
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
        }
    }
    eprintln!("mock: websocket {path} closed");
    Ok(())
}

// ---- simulated activity ---------------------------------------------------------------------

async fn generate_events(mock: Arc<Mock>, every: Duration) {
    let mut ticks = interval_at(Instant::now() + every, every);
    let mut cycle: u64 = 0;
    loop {
        cycle += 1;
        ticks.tick().await;
        let candidates: Vec<usize> =
            (0..CAMERAS.len()).filter(|&i| mock.connected(i) && !CAMERAS[i].types.is_empty()).collect();
        if let Some(camera) = mock.pick(&candidates)
            && let Some(kind) = mock.pick(CAMERAS[camera].types)
        {
            tokio::spawn(smart_detection(mock.clone(), camera, kind));
        }
        if cycle.is_multiple_of(3) && mock.connected(FRONT_DOOR) {
            package_drop(&mock, FRONT_DOOR);
        }
        if cycle.is_multiple_of(4) && mock.connected(FRONT_DOOR) {
            tokio::spawn(simple_event(mock.clone(), FRONT_DOOR, "ring", None, Duration::from_secs(2)));
        }
    }
}

/// add with one kind → 1.5s later an update growing the kinds → 6s later the end.
async fn smart_detection(mock: Arc<Mock>, camera: usize, kind: &'static str) {
    let id = mock.new_id();
    let device = CAMERAS[camera].id;
    mock.publish_event(json!({"type": "add", "item": {
        "id": id, "modelKey": "event", "type": "smartDetectZone", "start": now_ms(), "device": device,
        "smartDetectTypes": [kind],
    }}));
    sleep(Duration::from_millis(1500)).await;
    let types = CAMERAS[camera].types;
    let extra = ["person", "face"]
        .into_iter()
        .find(|k| *k != kind && types.contains(k))
        .or_else(|| types.iter().copied().find(|k| *k != kind));
    if let Some(extra) = extra {
        mock.publish_event(json!({"type": "update", "item": {
            "id": id, "modelKey": "event", "smartDetectTypes": [kind, extra],
        }}));
    }
    sleep(Duration::from_secs(6)).await;
    mock.publish_event(json!({"type": "update", "item": {"id": id, "modelKey": "event", "end": now_ms()}}));
}

/// A package detection arrives as one closed `update` with no preceding `add`.
fn package_drop(mock: &Mock, camera: usize) {
    let end = now_ms();
    mock.publish_event(json!({"type": "update", "item": {
        "id": mock.new_id(), "modelKey": "event", "type": "smartDetectZone", "start": end - 4000, "end": end,
        "device": CAMERAS[camera].id, "smartDetectTypes": ["package"],
    }}));
}

/// add (optionally with smartDetectTypes), then a partial update carrying only the end.
async fn simple_event(
    mock: Arc<Mock>,
    camera: usize,
    event_type: &'static str,
    types: Option<&'static [&'static str]>,
    duration: Duration,
) {
    let id = mock.new_id();
    let mut item = json!({
        "id": id, "modelKey": "event", "type": event_type, "start": now_ms(), "device": CAMERAS[camera].id,
    });
    if let Some(types) = types {
        item["smartDetectTypes"] = json!(types);
    }
    mock.publish_event(json!({"type": "add", "item": item}));
    sleep(duration).await;
    mock.publish_event(json!({"type": "update", "item": {"id": id, "modelKey": "event", "end": now_ms()}}));
}

fn trigger(mock: &Arc<Mock>, camera: Option<&str>, kind: &str) -> Reply {
    let Some(camera) = camera.and_then(camera_index) else {
        return json_reply(404, &json!({"error": "unknown camera", "name": "NOT_FOUND"}));
    };
    let (event_type, types, duration) = match kind {
        "package" => {
            package_drop(mock, camera);
            return json_reply(200, &json!({"ok": true, "camera": CAMERAS[camera].name, "kind": kind}));
        }
        "ring" => ("ring", None, Duration::from_secs(2)),
        "motion" => ("motion", None, Duration::from_secs(4)),
        "audio" => ("smartAudioDetect", Some(&["alrmSmoke"][..]), Duration::from_secs(3)),
        _ => {
            let Some(kind) = ["person", "vehicle", "animal", "face", "licensePlate"].into_iter().find(|k| *k == kind)
            else {
                return json_reply(400, &json!({"error": format!("unknown kind {kind}"), "name": "BAD_REQUEST"}));
            };
            tokio::spawn(smart_detection(mock.clone(), camera, kind));
            return json_reply(200, &json!({"ok": true, "camera": CAMERAS[camera].name, "kind": kind}));
        }
    };
    tokio::spawn(simple_event(mock.clone(), camera, event_type, types, duration));
    json_reply(200, &json!({"ok": true, "camera": CAMERAS[camera].name, "kind": kind}))
}

async fn toggle_side_gate(mock: Arc<Mock>) {
    let every = Duration::from_secs(60);
    let mut ticks = interval_at(Instant::now() + every, every);
    loop {
        ticks.tick().await;
        let connected = {
            let mut state = mock.state();
            state.connected[SIDE_GATE] = !state.connected[SIDE_GATE];
            state.connected[SIDE_GATE]
        };
        let frame = json!({"type": "update", "item": {
            "id": CAMERAS[SIDE_GATE].id, "modelKey": "camera",
            "state": if connected { "CONNECTED" } else { "DISCONNECTED" },
        }});
        eprintln!("mock: device {frame}");
        let _ = mock.devices.send(frame.to_string());
    }
}
