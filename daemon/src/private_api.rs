//! UniFi OS login (a local service account, no MFA) and Protect's private livestream endpoint.
//!
//! Protocol as implemented by hjdhjd/unifi-protect (src/transport/auth.ts, ws-endpoint.ts,
//! livestream-session.ts) and uilibs/uiprotect (api.py):
//! - `POST /api/auth/login` with `{username, password, rememberMe, token}`; the session is the
//!   `TOKEN` (or `UOS_TOKEN`) cookie plus a CSRF token from `X-Updated-CSRF-Token`, falling back
//!   to `X-CSRF-Token`, sent back as `X-CSRF-Token`. 401 = bad credentials, 499 with
//!   `MFA_AUTH_REQUIRED` = MFA, 429 = too many attempts.
//! - `GET /proxy/protect/api/ws/livestream?…` answers `{"url": "wss://…"}`; the URL carries its own
//!   token, and its hostname may be internal, so it is replaced with the configured host.

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use reqwest::header::{HeaderMap, RETRY_AFTER, SET_COOKIE};
use reqwest::{StatusCode, Url};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex, OnceCell};

use crate::config;
use crate::keyring;
use crate::protect::{Protect, error_chain};

const LOGIN_PATH: &str = "/api/auth/login";
const LIVESTREAM_PATH: &str = "/proxy/protect/api/ws/livestream";
/// UniFi OS answers an MFA-protected account with this non-standard status.
const MFA_STATUS: u16 = 499;
/// Largest livestream frame payload we ask for (the protocol's own limit is 2^24 - 1).
const CHUNK_SIZE: &str = "16384";
/// Protect cuts a fragment this often; it bounds the delay from camera to player.
const FRAGMENT_MILLIS: &str = "100";
/// After a failed automatic login, wait this long before the next one (UniFi OS locks accounts
/// after repeated failures). `sauron setup` and a config reload clear it.
const LOGIN_FAILURE_PAUSE: Duration = Duration::from_secs(15 * 60);
const RATE_LIMIT_PAUSE: Duration = Duration::from_secs(5 * 60);

/// Why a login failed, worded for humans.
#[derive(Debug)]
pub enum LoginError {
    BadCredentials,
    MfaRequired,
    RateLimited(Option<Duration>),
    /// A recent automatic login failed; not retrying yet.
    Paused { reason: String, remaining: Duration },
    Other(anyhow::Error),
}

impl fmt::Display for LoginError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoginError::BadCredentials => f.write_str("the console rejected the username or password"),
            LoginError::MfaRequired => {
                f.write_str("the account requires MFA; use a local-only UniFi user without MFA")
            }
            LoginError::RateLimited(Some(wait)) => {
                write!(f, "too many login attempts; the console asks to wait {}s", wait.as_secs())
            }
            LoginError::RateLimited(None) => f.write_str("too many login attempts; try again in a few minutes"),
            LoginError::Paused { reason, remaining } => write!(
                f,
                "not logging in again for {}s after: {reason} (run sauron setup to retry now)",
                remaining.as_secs()
            ),
            LoginError::Other(err) => write!(f, "{err:#}"),
        }
    }
}

impl std::error::Error for LoginError {}

/// A logged-in UniFi OS session, cached in `<runtime>/session` for every sauron process.
#[derive(Serialize, Deserialize, Clone)]
struct Session {
    host: String,
    username: String,
    cookie: String,
    csrf: Option<String>,
}

/// A failed automatic login, remembered in `<runtime>/login-pause` so no process retries it soon.
#[derive(Serialize, Deserialize)]
struct Pause {
    host: String,
    username: String,
    until_ms: u64,
    reason: String,
}

fn session_path() -> PathBuf {
    config::runtime_dir().join("session")
}

fn pause_path() -> PathBuf {
    config::runtime_dir().join("login-pause")
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

async fn read_json<T: for<'de> Deserialize<'de>>(path: &PathBuf) -> Option<T> {
    serde_json::from_slice(&tokio::fs::read(path).await.ok()?).ok()
}

/// Writes a private (0600) file atomically, creating the runtime dir if needed.
async fn write_private(path: &PathBuf, data: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        tokio::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).await?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let mut file =
        tokio::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp).await?;
    file.write_all(data).await?;
    drop(file);
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}

async fn remove(path: &PathBuf) {
    // Missing is the goal; any other error only means the next login finds a stale file.
    let _ = tokio::fs::remove_file(path).await;
}

/// Drops the cached session and any login pause (new config or credentials).
pub async fn forget_session() {
    remove(&session_path()).await;
    remove(&pause_path()).await;
}

/// Quality → livestream channel (0 is the highest quality).
pub fn channel(quality: &str) -> u8 {
    match quality {
        "medium" => 1,
        "low" => 2,
        _ => 0,
    }
}

/// Private-API client for one console and service account.
pub struct PrivateApi<'a> {
    protect: &'a Protect,
    host: String,
    username: String,
    password: String,
    session: Option<Session>,
}

impl<'a> PrivateApi<'a> {
    pub fn new(protect: &'a Protect, username: &str, password: String) -> Self {
        Self { protect, host: protect.console_id(), username: username.to_owned(), password, session: None }
    }

    /// Logs in now, ignoring any pause (an explicit user action), and caches the session.
    pub async fn login(&mut self) -> Result<(), LoginError> {
        match self.post_login().await {
            Ok(session) => {
                if let Err(err) = self.save(&session).await {
                    eprintln!("sauron: cannot cache the UniFi OS session: {err:#}");
                }
                remove(&pause_path()).await;
                self.session = Some(session);
                Ok(())
            }
            Err(err) => {
                match &err {
                    LoginError::BadCredentials | LoginError::MfaRequired => self.pause(LOGIN_FAILURE_PAUSE, &err).await,
                    LoginError::RateLimited(wait) => {
                        self.pause(wait.unwrap_or(RATE_LIMIT_PAUSE).max(Duration::from_secs(1)), &err).await;
                    }
                    LoginError::Paused { .. } | LoginError::Other(_) => {}
                }
                self.session = None;
                Err(err)
            }
        }
    }

    async fn pause(&self, wait: Duration, err: &LoginError) {
        let pause = Pause {
            host: self.host.clone(),
            username: self.username.clone(),
            until_ms: now_ms() + wait.as_millis() as u64,
            reason: err.to_string(),
        };
        if let Ok(json) = serde_json::to_vec(&pause) {
            let _ = write_private(&pause_path(), &json).await;
        }
    }

    async fn save(&self, session: &Session) -> Result<()> {
        write_private(&session_path(), &serde_json::to_vec(session)?).await.context("writing the session cache")
    }

    /// The cached session for this console and user, else a fresh login (unless paused).
    async fn session(&mut self) -> Result<&Session, LoginError> {
        if self.session.is_none() {
            let cached: Option<Session> = read_json(&session_path()).await;
            self.session = cached.filter(|s| s.host == self.host && s.username == self.username);
        }
        if self.session.is_none() {
            self.automatic_login().await?;
        }
        self.session.as_ref().ok_or_else(|| LoginError::Other(anyhow!("no session")))
    }

    /// A login not requested by the user: honours the pause left by an earlier failure.
    async fn automatic_login(&mut self) -> Result<(), LoginError> {
        if let Some(pause) = read_json::<Pause>(&pause_path()).await
            && pause.host == self.host
            && pause.username == self.username
            && pause.until_ms > now_ms()
        {
            let remaining = Duration::from_millis(pause.until_ms - now_ms());
            return Err(LoginError::Paused { reason: pause.reason, remaining });
        }
        self.login().await
    }

    async fn post_login(&self) -> Result<Session, LoginError> {
        let url = format!("{}{LOGIN_PATH}", self.protect.origin());
        let body = serde_json::json!({
            "username": self.username,
            "password": self.password,
            "rememberMe": true,
            "token": "",
        });
        let send = |csrf: Option<String>| {
            let mut request = self.protect.http().post(&url).json(&body);
            if let Some(csrf) = csrf {
                request = request.header("X-CSRF-Token", csrf);
            }
            async move { request.send().await.map_err(|e| LoginError::Other(anyhow!(error_chain(&e)))) }
        };
        let mut response = send(None).await?;
        // Some UniFi OS versions want a CSRF token even for the login; the root page hands one out.
        // Definite answers (bad credentials, MFA, rate limit) are not retried: each failed attempt
        // counts toward the console's lockout.
        let definite = matches!(response.status().as_u16(), 200..=299 | 401 | 429 | MFA_STATUS);
        if !definite && let Some(csrf) = self.root_csrf().await {
            response = send(Some(csrf)).await?;
        }

        let status = response.status();
        let headers = response.headers().clone();
        let body = response.bytes().await.unwrap_or_default();
        let code = serde_json::from_slice::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v.get("code").and_then(|c| c.as_str()).map(str::to_owned));
        if status.as_u16() == MFA_STATUS || code.as_deref() == Some("MFA_AUTH_REQUIRED") {
            return Err(LoginError::MfaRequired);
        }
        match status {
            StatusCode::UNAUTHORIZED => return Err(LoginError::BadCredentials),
            StatusCode::TOO_MANY_REQUESTS => return Err(LoginError::RateLimited(retry_after(&headers))),
            status if !status.is_success() => {
                return Err(LoginError::Other(anyhow!("login failed: HTTP {status}")));
            }
            _ => {}
        }
        let cookie = session_cookie(&headers)
            .ok_or_else(|| LoginError::Other(anyhow!("login succeeded but the console sent no session cookie")))?;
        Ok(Session { host: self.host.clone(), username: self.username.clone(), cookie, csrf: csrf_header(&headers) })
    }

    async fn root_csrf(&self) -> Option<String> {
        let response = self.protect.http().get(self.protect.origin()).send().await.ok()?;
        csrf_header(response.headers())
    }

    /// The websocket URL for a camera's livestream, host rewritten to the configured console.
    /// Logs in again once if the cached session has expired.
    pub async fn livestream_url(&mut self, camera: &str, channel: u8) -> Result<String> {
        let mut url = Url::parse(&format!("{}{LIVESTREAM_PATH}", self.protect.origin()))?;
        url.query_pairs_mut()
            .append_pair("allowPartialGOP", "")
            .append_pair("camera", camera)
            .append_pair("channel", &channel.to_string())
            .append_pair("chunkSize", CHUNK_SIZE)
            .append_pair("fragmentDurationMillis", FRAGMENT_MILLIS)
            .append_pair("lens", "0")
            .append_pair("progressive", "")
            .append_pair("rebaseTimestampsToZero", "true")
            .append_pair("requestId", &format!("sauron-{camera}-{channel}"))
            .append_pair("type", "fmp4")
            .append_pair("useWallClock", "false");

        let mut relogged = false;
        loop {
            let session = self.session().await?.clone();
            let mut request = self.protect.http().get(url.as_str()).header("Cookie", &session.cookie);
            if let Some(csrf) = &session.csrf {
                request = request.header("X-CSRF-Token", csrf);
            }
            let response = request.send().await.map_err(|e| anyhow!(error_chain(&e)))?;
            let status = response.status();
            if status == StatusCode::UNAUTHORIZED && !relogged {
                relogged = true;
                self.session = None;
                remove(&session_path()).await;
                self.automatic_login().await?;
                continue;
            }
            if let Some(csrf) = response.headers().get("x-updated-csrf-token").and_then(|v| v.to_str().ok())
                && session.csrf.as_deref() != Some(csrf)
            {
                let session = Session { csrf: Some(csrf.to_owned()), ..session };
                let _ = self.save(&session).await;
                self.session = Some(session);
            }
            let body = response.bytes().await.map_err(|e| anyhow!(error_chain(&e)))?;
            match status {
                StatusCode::FORBIDDEN => {
                    return Err(anyhow!("the UniFi user may not view Protect (give it Protect: View Only)"));
                }
                status if !status.is_success() => return Err(anyhow!("{LIVESTREAM_PATH}: HTTP {status}")),
                _ => {}
            }
            #[derive(Deserialize)]
            struct Endpoint {
                url: String,
            }
            let endpoint: Endpoint =
                serde_json::from_slice(&body).with_context(|| format!("{LIVESTREAM_PATH}: unexpected response"))?;
            let mut ws = Url::parse(&endpoint.url).with_context(|| format!("invalid livestream URL {:?}", endpoint.url))?;
            ws.set_host(Some(self.protect.hostname())).context("cannot rewrite the livestream host")?;
            return Ok(ws.into());
        }
    }
}

fn csrf_header(headers: &HeaderMap) -> Option<String> {
    ["x-updated-csrf-token", "x-csrf-token"]
        .into_iter()
        .find_map(|name| headers.get(name)?.to_str().ok())
        .map(str::to_owned)
}

/// `TOKEN=…` (or `UOS_TOKEN=…`) from the login response, without cookie attributes.
fn session_cookie(headers: &HeaderMap) -> Option<String> {
    let pairs: Vec<&str> = headers
        .get_all(SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .filter_map(|v| v.split(';').next())
        .map(str::trim)
        .filter(|pair| pair.contains('='))
        .collect();
    pairs
        .iter()
        .find(|pair| pair.starts_with("TOKEN=") || pair.starts_with("UOS_TOKEN="))
        .or(pairs.first())
        .map(|pair| (*pair).to_owned())
}

fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    headers.get(RETRY_AFTER)?.to_str().ok()?.trim().parse().ok().map(Duration::from_secs)
}

/// Keeps a private-API session cached for the daemon, so clicking an alert starts video
/// without a login round trip. Re-created on every config reload.
pub struct Warmer {
    client: Arc<Protect>,
    username: String,
    password: OnceCell<Option<String>>,
    busy: Mutex<()>,
}

impl Warmer {
    pub fn new(client: Arc<Protect>, username: String) -> Self {
        Self { client, username, password: OnceCell::new(), busy: Mutex::new(()) }
    }

    /// Logs in unless a session is cached; quiet when instant live is unconfigured or paused.
    pub async fn warm(&self) -> Result<()> {
        let _busy = self.busy.lock().await;
        let console = self.client.console_id();
        let password = self.password.get_or_init(|| keyring::lookup(&console, &self.username)).await;
        let Some(password) = password else { return Ok(()) };
        let mut api = PrivateApi::new(&self.client, &self.username, password.clone());
        match api.session().await {
            Ok(_) | Err(LoginError::Paused { .. }) => Ok(()),
            Err(err) => Err(err.into()),
        }
    }
}
