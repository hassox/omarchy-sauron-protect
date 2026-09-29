//! Mock UniFi Protect console for end-to-end testing without cameras: the Integration API, the
//! UniFi OS login and Protect's private livestream.
//!
//! `cargo run --example mock_protect -- [--port 7447] [--interval 20]`, then point sauron at
//! `host = "http://127.0.0.1:7447"` with `api_key = "mock"`; the UniFi OS user is `sauron` with
//! password `mock` (user `mfa`, same password, behaves like an account with MFA enabled).
//! `curl -X POST 'http://127.0.0.1:7447/mock/trigger?camera=Driveway&kind=person'` fires an event;
//! `POST /mock/livestream?fail=true|false` breaks or repairs the livestream endpoint;
//! `POST /mock/expire-sessions` logs every UniFi OS session out.

use std::collections::{HashMap, HashSet, VecDeque};
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
use tokio::sync::{broadcast, watch};
use tokio::time::{Instant, interval_at, sleep};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{Callback, ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http;

const API_KEY: &str = "mock";
const API: &str = "/proxy/protect/integration";
const UNAUTHENTICATED: &str =
    r#"{"error":"Failed to authenticate request using 'apiKey'","name":"UNAUTHENTICATED","type":"apiKey"}"#;
const MAX_HEAD: usize = 16 * 1024;
const LOGIN_USER: &str = "sauron";
const MFA_USER: &str = "mfa";
const LOGIN_PASSWORD: &str = "mock";
/// Failed logins allowed per minute before the console answers 429.
const LOGIN_FAILURES_ALLOWED: usize = 5;
const LOGIN_WINDOW: Duration = Duration::from_secs(60);
const LIVESTREAM_PATH: &str = "/proxy/protect/api/ws/livestream";
/// Where the minted websocket URL points; the host is deliberately unreachable so clients must
/// rewrite it to the console address, as on multi-homed consoles.
const LIVESTREAM_WS_PATH: &str = "/ws/livestream";
const LIVESTREAM_INTERNAL_HOST: &str = "unifi.internal";

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
/// Answers 400 to `highQuality=true` snapshots, like the G5 Pro (supportFullHdSnapshot false).
const DRIVEWAY: usize = 1;
const SIDE_GATE: usize = 4;

struct State {
    connected: [bool; CAMERAS.len()],
    streams: HashSet<(usize, String)>,
    rng: u64,
    /// UniFi OS session cookies (the `TOKEN` value) that are logged in.
    sessions: HashSet<String>,
    login_failures: VecDeque<Instant>,
    /// Single-use livestream websocket tokens.
    livestream_tokens: HashMap<String, LivestreamTarget>,
    livestream_broken: bool,
}

struct Mock {
    state: Mutex<State>,
    events: broadcast::Sender<String>,
    devices: broadcast::Sender<String>,
    drawtext: bool,
    port: u16,
    livestreams: Mutex<HashMap<(usize, u8), Arc<Livestream>>>,
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

    fn new_uuid(&self) -> String {
        let (a, b) = (self.random(), self.random());
        format!("{:08x}-{:04x}-4{:03x}-a{:03x}-{:012x}", a >> 32, a & 0xffff, b >> 52, (b >> 40) & 0xfff, b & 0xffff_ffff_ffff)
    }

    /// The encoder for a camera channel, started on first use and kept running for later viewers.
    fn livestream(self: &Arc<Self>, camera: usize, channel: u8) -> Arc<Livestream> {
        let mut livestreams = self.livestreams.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        livestreams
            .entry((camera, channel))
            .or_insert_with(|| {
                let stream = Arc::new(Livestream {
                    cache: Mutex::new(Gop { init: Vec::new(), fragments: Vec::new() }),
                    live: broadcast::channel(256).0,
                    phase: watch::channel(Phase::Starting).0,
                });
                tokio::spawn(encode(self.clone(), camera, channel, stream.clone()));
                stream
            })
            .clone()
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
                "supportFullHdSnapshot": index != DRIVEWAY,
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
            sessions: HashSet::new(),
            login_failures: VecDeque::new(),
            livestream_tokens: HashMap::new(),
            livestream_broken: false,
        }),
        events: broadcast::channel(64).0,
        devices: broadcast::channel(64).0,
        drawtext: has_drawtext().await,
        port,
        livestreams: Mutex::new(HashMap::new()),
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
    if is_upgrade && url.path() == LIVESTREAM_WS_PATH {
        // The URL's token is the only credential, and it works once.
        let token = url.query_pairs().find(|(k, _)| k == "token").map(|(_, v)| v.into_owned()).unwrap_or_default();
        let Some(target) = mock.state().livestream_tokens.remove(&token) else {
            eprintln!("mock: livestream websocket rejected: unknown token");
            return respond(&mut stream, &json_reply(401, &json!({"error": "invalid token"}))).await;
        };
        let replay = Rewind { prefix: buf, pos: 0, inner: stream };
        return livestream_socket(mock.clone(), replay, target).await;
    }
    if is_upgrade {
        let topic = match url.path().strip_prefix(API) {
            Some("/v1/subscribe/events") => &mock.events,
            Some("/v1/subscribe/devices") => &mock.devices,
            _ => return respond(&mut stream, &not_found()).await,
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
    let reply = route(mock, &head, &url, body).await;
    eprintln!("mock: {} {} -> {}", head.method, head.target, reply.status);
    respond(&mut stream, &reply).await
}

async fn respond(stream: &mut TcpStream, reply: &Reply) -> io::Result<()> {
    let reason = match reply.status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        429 => "Too Many Requests",
        499 => "Client Closed Request",
        503 => "Service Unavailable",
        _ => "Internal Server Error",
    };
    let mut head = format!(
        "HTTP/1.1 {} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        reply.status,
        reply.content_type,
        reply.body.len()
    );
    for (name, value) in &reply.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(&reply.body).await?;
    stream.shutdown().await
}

struct Reply {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
    headers: Vec<(&'static str, String)>,
}

impl Reply {
    fn new(status: u16, content_type: &'static str, body: Vec<u8>) -> Self {
        Self { status, content_type, body, headers: Vec::new() }
    }

    fn header(mut self, name: &'static str, value: String) -> Self {
        self.headers.push((name, value));
        self
    }
}

fn json_reply(status: u16, value: &Value) -> Reply {
    Reply::new(status, "application/json", value.to_string().into_bytes())
}

fn not_found() -> Reply {
    json_reply(404, &json!({"error": "Not found", "name": "NOT_FOUND"}))
}

async fn route(mock: &Arc<Mock>, head: &Head, url: &reqwest::Url, body: &[u8]) -> Reply {
    let query = |key: &str| url.query_pairs().find(|(k, _)| k == key).map(|(_, v)| v.into_owned());
    match (head.method.as_str(), url.path()) {
        ("POST", "/mock/trigger") => {
            return trigger(mock, query("camera").as_deref(), query("kind").as_deref().unwrap_or("person"));
        }
        ("POST", "/mock/livestream") => {
            let broken = query("fail").as_deref() == Some("true");
            mock.state().livestream_broken = broken;
            return json_reply(200, &json!({"ok": true, "livestreamBroken": broken}));
        }
        ("POST", "/mock/expire-sessions") => {
            mock.state().sessions.clear();
            return json_reply(200, &json!({"ok": true}));
        }
        // UniFi OS hands out a CSRF token with its root page.
        ("GET", "/") => {
            return Reply::new(200, "text/html", b"<!doctype html><title>UniFi OS</title>".to_vec())
                .header("X-CSRF-Token", mock.new_uuid());
        }
        ("POST", "/api/auth/login") => return login(mock, body),
        ("GET", LIVESTREAM_PATH) => return livestream_endpoint(mock, head, &query),
        _ => {}
    }
    let Some(path) = url.path().strip_prefix(API) else { return not_found() };
    if !head.authorized() {
        return Reply::new(401, "application/json", UNAUTHENTICATED.as_bytes().to_vec());
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
            let high_quality = query("highQuality").as_deref() == Some("true");
            if high_quality && index == DRIVEWAY {
                return json_reply(400, &json!({"error": "Full HD snapshots are not supported", "name": "BAD_REQUEST"}));
            }
            match render_snapshot(mock, index, high_quality).await {
                Ok(jpeg) => Reply::new(200, "image/jpeg", jpeg),
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

// ---- UniFi OS login and the private livestream ------------------------------------------------

/// `POST /api/auth/login`: session cookie + CSRF header on success; 401 for bad credentials, 499
/// `MFA_AUTH_REQUIRED` for an MFA account, 429 after too many failures (UniFi OS behaviour as
/// seen by hjdhjd/unifi-protect, uiprotect, aiounifi and unifi-cli).
fn login(mock: &Mock, body: &[u8]) -> Reply {
    let request: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let username = request["username"].as_str().unwrap_or_default();
    let password = request["password"].as_str().unwrap_or_default();
    let (token, csrf, user_id) = (format!("mock.{}.{}", mock.new_id(), mock.new_id()), mock.new_uuid(), mock.new_uuid());

    let mut state = mock.state();
    let now = Instant::now();
    while state.login_failures.front().is_some_and(|at| now.duration_since(*at) > LOGIN_WINDOW) {
        state.login_failures.pop_front();
    }
    if state.login_failures.len() >= LOGIN_FAILURES_ALLOWED {
        return json_reply(
            429,
            &json!({"code": "AUTHENTICATION_FAILED_LIMIT_REACHED", "message": "You've reached the login attempt limit"}),
        );
    }
    if password != LOGIN_PASSWORD || !matches!(username, LOGIN_USER | MFA_USER) {
        state.login_failures.push_back(now);
        return json_reply(
            401,
            &json!({"code": "AUTHENTICATION_FAILED_INVALID_CREDENTIALS", "message": "Invalid username or password"}),
        );
    }
    if username == MFA_USER {
        return json_reply(
            499,
            &json!({
                "code": "MFA_AUTH_REQUIRED",
                "message": "MFA Authentication Required",
                "data": {"mfaCookie": format!("UBIC_2FA={user_id}"), "authenticators": [{"id": user_id, "type": "totp"}]},
            }),
        );
    }
    state.sessions.insert(token.clone());
    json_reply(200, &json!({"unique_id": user_id, "username": username, "isOwner": false, "deviceToken": ""}))
        .header("Set-Cookie", format!("TOKEN={token}; path=/; samesite=strict; secure; httponly"))
        .header("X-Updated-CSRF-Token", csrf)
}

struct LivestreamTarget {
    camera: usize,
    channel: u8,
    chunk_size: usize,
}

/// `GET /proxy/protect/api/ws/livestream?camera=…&channel=…&type=fmp4…` → `{"url": "ws://…"}`, a
/// single-use URL on an internal hostname (clients rewrite the host and keep port and token).
fn livestream_endpoint(mock: &Mock, head: &Head, query: &dyn Fn(&str) -> Option<String>) -> Reply {
    let cookie = head
        .header("cookie")
        .and_then(|cookies| cookies.split(';').map(str::trim).find_map(|pair| pair.strip_prefix("TOKEN=")));
    let token = mock.new_id();
    let mut state = mock.state();
    if !cookie.is_some_and(|cookie| state.sessions.contains(cookie)) {
        return json_reply(401, &json!({"error": "Unauthorized"}));
    }
    if state.livestream_broken {
        return json_reply(500, &json!({"error": "Livestream unavailable (mock failure)"}));
    }
    let Some(camera) = query("camera").and_then(|id| CAMERAS.iter().position(|c| c.id == id)) else {
        return json_reply(400, &json!({"error": "Invalid camera"}));
    };
    if !state.connected[camera] {
        return json_reply(503, &json!({"error": "Camera is not connected"}));
    }
    let Some(channel) = query("channel").and_then(|c| c.parse::<u8>().ok()).filter(|c| *c <= 2) else {
        return json_reply(400, &json!({"error": "Invalid channel"}));
    };
    if query("type").as_deref() != Some("fmp4") {
        return json_reply(400, &json!({"error": "Only fmp4 is supported by the mock"}));
    }
    let chunk_size = query("chunkSize").and_then(|c| c.parse().ok()).filter(|c| (256..=1 << 20).contains(c));
    let target = LivestreamTarget { camera, channel, chunk_size: chunk_size.unwrap_or(4096) };
    state.livestream_tokens.insert(token.clone(), target);
    let url = format!("ws://{LIVESTREAM_INTERNAL_HOST}:{}{LIVESTREAM_WS_PATH}?token={token}", mock.port);
    json_reply(200, &json!({ "url": url }))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Starting,
    Ready,
    Ended,
}

struct Fragment {
    moof: Vec<u8>,
    mdat: Vec<u8>,
}

/// The init segment and every fragment since the last keyframe: what a new viewer gets at once.
struct Gop {
    init: Vec<u8>,
    fragments: Vec<Arc<Fragment>>,
}

/// One continuously encoding camera channel. `live` carries new fragments (`None` = encoder died).
struct Livestream {
    cache: Mutex<Gop>,
    live: broadcast::Sender<Option<Arc<Fragment>>>,
    phase: watch::Sender<Phase>,
}

impl Livestream {
    fn cache(&self) -> std::sync::MutexGuard<'_, Gop> {
        self.cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

async fn encode(mock: Arc<Mock>, camera: usize, channel: u8, stream: Arc<Livestream>) {
    if let Err(err) = encode_fmp4(&mock, camera, channel, &stream).await {
        eprintln!("mock: livestream {} channel {channel}: {err}", CAMERAS[camera].name);
    }
    mock.livestreams.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).remove(&(camera, channel));
    stream.phase.send_replace(Phase::Ended);
    let _ = stream.live.send(None);
}

/// Runs ffmpeg in real time and splits its fragmented MP4 into the init segment and moof+mdat
/// fragments, keeping the current GOP cached.
async fn encode_fmp4(mock: &Mock, camera: usize, channel: u8, stream: &Livestream) -> io::Result<()> {
    let def = &CAMERAS[camera];
    let size = match channel {
        0 => "1280x720",
        1 => "960x540",
        _ => "640x360",
    };
    let filter = if mock.drawtext {
        format!(
            "drawtext=text='{} · live  %{{localtime\\:%T}}':fontcolor=white:fontsize=h/12:box=1:boxcolor=black@0.6:boxborderw=8:x=(w-tw)/2:y=(h-th)/2",
            def.name
        )
    } else {
        format!("hue=h={}", def.hue)
    };
    let mut ffmpeg = tokio::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-re", "-f", "lavfi", "-i"])
        .arg(format!("testsrc2=size={size}:rate=30"))
        .args(["-vf", &filter, "-c:v", "libx264", "-preset", "ultrafast", "-tune", "zerolatency"])
        .args(["-g", "30", "-pix_fmt", "yuv420p", "-f", "mp4"])
        .args(["-movflags", "frag_keyframe+empty_moov+default_base_moof", "-frag_duration", "100000", "pipe:1"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdout = ffmpeg.stdout.take().ok_or_else(|| io::Error::other("ffmpeg has no stdout"))?;
    eprintln!("mock: livestream {} channel {channel} encoding", def.name);

    let (mut buf, mut init, mut moof) = (Vec::with_capacity(1 << 20), Vec::new(), None::<Vec<u8>>);
    loop {
        let mut offset = 0;
        while let Some((kind, size)) = box_header(&buf[offset..]) {
            let Some(item) = buf.get(offset..offset + size) else { break };
            match &kind {
                b"ftyp" => init.extend_from_slice(item),
                b"moov" => {
                    init.extend_from_slice(item);
                    stream.cache().init = std::mem::take(&mut init);
                    stream.phase.send_replace(Phase::Ready);
                }
                b"moof" => moof = Some(item.to_vec()),
                b"mdat" => {
                    if let Some(moof) = moof.take() {
                        let keyframe = first_sample_is_sync(&moof);
                        let fragment = Arc::new(Fragment { moof, mdat: item.to_vec() });
                        let mut cache = stream.cache();
                        if keyframe {
                            cache.fragments.clear();
                        }
                        if keyframe || !cache.fragments.is_empty() {
                            cache.fragments.push(fragment.clone());
                        }
                        // Sent under the cache lock so a joining viewer sees each fragment once.
                        let _ = stream.live.send(Some(fragment));
                    }
                }
                _ => {}
            }
            offset += size;
        }
        buf.drain(..offset);
        if stdout.read_buf(&mut buf).await? == 0 {
            break;
        }
    }
    let status = ffmpeg.wait().await?;
    Err(io::Error::other(format!("ffmpeg exited ({status})")))
}

/// Type and total size of the ISO-BMFF box at the start of `data`, once its header is complete.
fn box_header(data: &[u8]) -> Option<([u8; 4], usize)> {
    let size = u32::from_be_bytes(data.get(0..4)?.try_into().ok()?) as usize;
    let kind: [u8; 4] = data.get(4..8)?.try_into().ok()?;
    let size = if size == 1 { u64::from_be_bytes(data.get(8..16)?.try_into().ok()?) as usize } else { size };
    (size >= 8).then_some((kind, size))
}

/// Payload of the first child box of type `kind` within `data` (a sequence of boxes).
fn child<'a>(mut data: &'a [u8], kind: &[u8; 4]) -> Option<&'a [u8]> {
    while let Some((found, size)) = box_header(data) {
        let item = data.get(..size)?;
        if &found == kind {
            return item.get(8..);
        }
        data = &data[size..];
    }
    None
}

/// Whether a fragment starts on a keyframe: the first sample's `sample_is_non_sync_sample` flag,
/// from trun (first-sample or per-sample flags) or the tfhd default.
fn first_sample_is_sync(moof: &[u8]) -> bool {
    let read = |data: &[u8], at: usize| data.get(at..at + 4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
    let is_sync = |flags: u32| flags & 0x0001_0000 == 0;
    let Some(traf) = moof.get(8..).and_then(|boxes| child(boxes, b"traf")) else { return false };
    if let Some(trun) = child(traf, b"trun") {
        let flags = read(trun, 0).unwrap_or(0) & 0x00ff_ffff;
        let mut at = 8 + if flags & 0x1 != 0 { 4 } else { 0 };
        if flags & 0x4 != 0 {
            return read(trun, at).is_some_and(is_sync);
        }
        if flags & 0x400 != 0 {
            at += if flags & 0x100 != 0 { 4 } else { 0 } + if flags & 0x200 != 0 { 4 } else { 0 };
            return read(trun, at).is_some_and(is_sync);
        }
    }
    let Some(tfhd) = child(traf, b"tfhd") else { return false };
    let flags = read(tfhd, 0).unwrap_or(0) & 0x00ff_ffff;
    if flags & 0x20 == 0 {
        return false;
    }
    let at = 8
        + if flags & 0x1 != 0 { 8 } else { 0 }
        + [0x2, 0x8, 0x10].iter().filter(|bit| flags & *bit != 0).count() * 4;
    read(tfhd, at).is_some_and(is_sync)
}

/// Livestream wire framing: `[type u8][length u24 BE][payload]` (hjdhjd/unifi-protect
/// src/transport/livestream-session.ts).
mod frame {
    pub const CODEC: u8 = 248;
    pub const BEGIN: u8 = 249;
    pub const INIT: u8 = 250;
    pub const MOOF: u8 = 251;
    pub const MDAT: u8 = 254;
    pub const END: u8 = 255;
}

fn push_frame(out: &mut Vec<u8>, kind: u8, payload: &[u8]) {
    out.push(kind);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes()[1..]);
    out.extend_from_slice(payload);
}

/// One media segment: begin, moof and mdat in `chunk_size` pieces, end.
fn push_fragment(out: &mut Vec<u8>, fragment: &Fragment, chunk_size: usize) {
    push_frame(out, frame::BEGIN, &[]);
    for piece in fragment.moof.chunks(chunk_size) {
        push_frame(out, frame::MOOF, piece);
    }
    for piece in fragment.mdat.chunks(chunk_size) {
        push_frame(out, frame::MDAT, piece);
    }
    push_frame(out, frame::END, &[]);
}

/// Sends `out` as binary messages whose boundaries deliberately ignore frame boundaries, so
/// clients must reassemble frames across messages.
async fn flush<S>(ws: &mut tokio_tungstenite::WebSocketStream<S>, out: &mut Vec<u8>, chunk_size: usize) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    for piece in out.chunks(chunk_size + 1000) {
        ws.send(Message::binary(piece.to_vec())).await.map_err(io::Error::other)?;
    }
    out.clear();
    Ok(())
}

/// A livestream viewer: codec string, init segment and the cached GOP at once, then live fragments.
async fn livestream_socket(mock: Arc<Mock>, stream: Rewind, target: LivestreamTarget) -> io::Result<()> {
    let mut ws = tokio_tungstenite::accept_async(stream).await.map_err(io::Error::other)?;
    let name = CAMERAS[target.camera].name;
    eprintln!("mock: livestream {name} channel {} open", target.channel);
    let source = mock.livestream(target.camera, target.channel);
    let mut phase = source.phase.subscribe();
    if !phase.wait_for(|p| *p != Phase::Starting).await.is_ok_and(|p| *p == Phase::Ready) {
        return ws.close(None).await.map_err(io::Error::other);
    }
    let (init, cached, mut live) = {
        let cache = source.cache();
        (cache.init.clone(), cache.fragments.clone(), source.live.subscribe())
    };
    let mut out = Vec::with_capacity(init.len() + 64 * 1024);
    push_frame(&mut out, frame::CODEC, b"avc1.64001f");
    push_frame(&mut out, frame::INIT, &init);
    for fragment in &cached {
        push_fragment(&mut out, fragment, target.chunk_size);
    }
    flush(&mut ws, &mut out, target.chunk_size).await?;
    loop {
        tokio::select! {
            fragment = live.recv() => match fragment {
                Ok(Some(fragment)) => {
                    push_fragment(&mut out, &fragment, target.chunk_size);
                    if flush(&mut ws, &mut out, target.chunk_size).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Ok(None) | Err(broadcast::error::RecvError::Closed) => {
                    let _ = ws.close(None).await;
                    break;
                }
            },
            incoming = ws.next() => match incoming {
                Some(Ok(Message::Close(_)) | Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
        }
    }
    eprintln!("mock: livestream {name} channel {} closed", target.channel);
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
