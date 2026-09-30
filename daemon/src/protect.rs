//! UniFi Protect Integration API client: REST calls and websocket subscriptions.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use reqwest::{Method, StatusCode, Url};
use rustls::ClientConfig;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use tokio::net::TcpStream;
use tokio::sync::Semaphore;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{self, Bytes};
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};

use crate::config::Config;
use crate::tls::{self, Rejection};

const API_PATH: &str = "/proxy/protect/integration";
const MAX_CONCURRENT_REQUESTS: usize = 4;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const LIVENESS_TIMEOUT: Duration = Duration::from_secs(3);
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(6);
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const H2_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);
const H2_KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(5);

pub type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// The console rejected the API key (HTTP 401).
#[derive(Debug)]
pub struct AuthError;

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("API key rejected (401)")
    }
}

impl std::error::Error for AuthError {}

pub fn is_auth(err: &anyhow::Error) -> bool {
    err.downcast_ref::<AuthError>().is_some()
}

#[derive(Deserialize, Clone, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct Camera {
    pub id: String,
    pub name: Option<String>,
    pub state: Option<String>,
    #[serde(rename = "type")]
    pub model: Option<String>,
    pub feature_flags: Option<FeatureFlags>,
}

#[derive(Deserialize, Clone, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct FeatureFlags {
    pub smart_detect_types: Option<Vec<String>>,
    pub smart_detect_audio_types: Option<Vec<String>>,
    pub support_full_hd_snapshot: Option<bool>,
}

impl Camera {
    pub fn model(&self) -> &str {
        self.model.as_deref().unwrap_or("")
    }

    /// Name, falling back to model, then id.
    pub fn display_name(&self) -> &str {
        [self.name.as_deref(), self.model.as_deref()]
            .into_iter()
            .flatten()
            .find(|s| !s.trim().is_empty())
            .unwrap_or(&self.id)
    }

    pub fn online(&self) -> bool {
        self.state.as_deref() == Some("CONNECTED")
    }

    /// Whether `highQuality=true` snapshots work (Protect answers 400 on cameras without it).
    pub fn supports_full_hd_snapshot(&self) -> bool {
        self.feature_flags
            .as_ref()
            .and_then(|f| f.support_full_hd_snapshot)
            == Some(true)
    }

    /// Detection kinds this camera can produce.
    pub fn kinds(&self) -> Vec<String> {
        let flags = self.feature_flags.as_ref();
        let mut kinds: Vec<String> = flags
            .and_then(|f| f.smart_detect_types.clone())
            .unwrap_or_default();
        if flags
            .and_then(|f| f.smart_detect_audio_types.as_ref())
            .is_some_and(|a| !a.is_empty())
        {
            kinds.push("audio".into());
        }
        kinds.push("motion".into());
        if self.model().to_ascii_lowercase().contains("doorbell") {
            kinds.push("ring".into());
        }
        kinds
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Streams {
    high: Option<String>,
    medium: Option<String>,
    low: Option<String>,
}

impl Streams {
    fn get(&self, quality: &str) -> Option<&str> {
        match quality {
            "high" => self.high.as_deref(),
            "medium" => self.medium.as_deref(),
            "low" => self.low.as_deref(),
            _ => None,
        }
    }
}

/// Websocket envelope shared by both subscriptions.
#[derive(Deserialize)]
pub struct Frame<T> {
    #[serde(rename = "type")]
    pub action: String,
    pub item: T,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct EventItem {
    pub id: Option<String>,
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub start: Option<i64>,
    pub end: Option<i64>,
    pub device: Option<String>,
    pub smart_detect_types: Option<Vec<String>>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct DeviceItem {
    pub id: Ids,
    pub model_key: Option<String>,
    pub name: Option<String>,
    pub state: Option<String>,
}

/// Device ids arrive as a single string or, for bulk frames, as an array.
#[derive(Deserialize)]
#[serde(untagged)]
pub enum Ids {
    One(String),
    Many(Vec<String>),
}

impl Default for Ids {
    fn default() -> Self {
        Ids::Many(Vec::new())
    }
}

impl Ids {
    pub fn as_slice(&self) -> &[String] {
        match self {
            Ids::One(id) => std::slice::from_ref(id),
            Ids::Many(ids) => ids,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MetaInfo {
    application_version: String,
}

/// How the connection to the console is secured, for `sauron check`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Security {
    Pinned,
    Trusted,
    Plain,
}

impl fmt::Display for Security {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Security::Pinned => "certificate pinned",
            Security::Trusted => "certificate trusted",
            Security::Plain => "no TLS",
        })
    }
}

/// Client for one address of the console. Everything in a session (REST, websockets, private
/// API, stream URLs) goes to this address; the console's identity stays the configured `host`.
pub struct Protect {
    http: reqwest::Client,
    base: Url,
    api_key: String,
    address: String,
    hostname: String,
    console_id: String,
    fallback: bool,
    security: Security,
    ws_tls: Arc<ClientConfig>,
    permits: Semaphore,
}

impl Protect {
    /// Client for the console at `address` (its `host` or a fallback); an empty API key sends no
    /// key (setup's console probe).
    pub fn new(config: &Config, address: &str) -> Result<Self> {
        let base = base_url(address)?;
        let hostname = base
            .host_str()
            .context("the address has no hostname")?
            .to_owned();
        let tls = tls::client_config(config.cert_sha256)?;
        let security = match (base.scheme(), config.cert_sha256) {
            ("http", _) => Security::Plain,
            (_, Some(_)) => Security::Pinned,
            (_, None) => Security::Trusted,
        };

        let mut http_tls = tls.clone();
        http_tls.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        let http = reqwest::Client::builder()
            .tls_backend_preconfigured(http_tls)
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .pool_idle_timeout(POOL_IDLE_TIMEOUT)
            .http2_keep_alive_interval(H2_KEEPALIVE_INTERVAL)
            .http2_keep_alive_timeout(H2_KEEPALIVE_TIMEOUT)
            .http2_keep_alive_while_idle(true)
            .build()
            .context("cannot build HTTP client")?;

        let mut ws_tls = tls;
        ws_tls.alpn_protocols = vec![b"http/1.1".to_vec()];

        Ok(Self {
            http,
            base,
            api_key: config.api_key.clone(),
            address: address.to_owned(),
            hostname,
            console_id: console_id(&config.host)?,
            fallback: address != config.host,
            security,
            ws_tls: Arc::new(ws_tls),
            permits: Semaphore::new(MAX_CONCURRENT_REQUESTS),
        })
    }

    /// The address this client talks to, as configured.
    pub fn address(&self) -> &str {
        &self.address
    }

    /// The active address when it is a fallback rather than `host`.
    pub fn via(&self) -> Option<&str> {
        self.fallback.then_some(self.address.as_str())
    }

    pub fn security(&self) -> Security {
        self.security
    }

    /// Hostname of the active address, used to rewrite stream URLs.
    pub fn hostname(&self) -> &str {
        &self.hostname
    }

    /// `host[:port]` of the configured `host`: the keyring and session-cache identity, whichever
    /// address is active.
    pub fn console_id(&self) -> &str {
        &self.console_id
    }

    /// `scheme://host[:port]` of the active address, for the UniFi OS (non-integration) endpoints.
    pub fn origin(&self) -> String {
        self.base.origin().ascii_serialization()
    }

    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// TLS settings for websocket connections to the console.
    pub fn ws_connector(&self) -> Connector {
        Connector::Rustls(self.ws_tls.clone())
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base.as_str().trim_end_matches('/'))
    }

    /// Status of an unauthenticated `meta/info` request: 401 means a Protect console answered.
    pub async fn probe(&self) -> Result<StatusCode> {
        let response = self
            .http
            .get(self.url("/v1/meta/info"))
            .send()
            .await
            .map_err(transport)?;
        Ok(response.status())
    }

    /// A quick `meta/info` round trip: is the console still there at this address?
    pub async fn alive(&self) -> Result<()> {
        self.request(Method::GET, "/v1/meta/info", None, LIVENESS_TIMEOUT)
            .await
            .map(drop)
    }

    /// Sends one request under the concurrency cap and reads the whole body.
    async fn request(
        &self,
        method: Method,
        path: &str,
        json: Option<&serde_json::Value>,
        timeout: Duration,
    ) -> Result<Bytes> {
        let _permit = self.permits.acquire().await?;
        let mut request = self.http.request(method, self.url(path)).timeout(timeout);
        if !self.api_key.is_empty() {
            request = request.header("X-API-KEY", &self.api_key);
        }
        if let Some(body) = json {
            request = request.json(body);
        }
        let response = request.send().await.map_err(transport)?;
        let status = response.status();
        let body = response.bytes().await.map_err(transport)?;
        if status == StatusCode::UNAUTHORIZED {
            return Err(AuthError.into());
        }
        if !status.is_success() {
            return Err(HttpError {
                path: path.to_owned(),
                status,
                message: api_error_message(&body),
            }
            .into());
        }
        Ok(body)
    }

    async fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let body = self
            .request(Method::GET, path, None, REQUEST_TIMEOUT)
            .await?;
        serde_json::from_slice(&body).with_context(|| format!("{path}: unexpected response"))
    }

    /// Protect application version; also validates the key.
    pub async fn version(&self) -> Result<String> {
        Ok(self
            .get_json::<MetaInfo>("/v1/meta/info")
            .await?
            .application_version)
    }

    pub async fn cameras(&self) -> Result<Vec<Camera>> {
        self.get_json("/v1/cameras").await
    }

    pub async fn snapshot(&self, camera: &str, high_quality: bool) -> Result<Bytes> {
        let path = format!("/v1/cameras/{camera}/snapshot?highQuality={high_quality}");
        self.request(Method::GET, &path, None, SNAPSHOT_TIMEOUT)
            .await
    }

    async fn streams(&self, camera: &str) -> Result<Streams> {
        self.get_json(&format!("/v1/cameras/{camera}/rtsps-stream"))
            .await
    }

    /// Creates (or returns the existing) stream of `quality`.
    async fn create_stream(&self, camera: &str, quality: &str) -> Result<String> {
        let path = format!("/v1/cameras/{camera}/rtsps-stream");
        let body = serde_json::json!({ "qualities": [quality] });
        let body = self
            .request(Method::POST, &path, Some(&body), REQUEST_TIMEOUT)
            .await?;
        let streams: Streams = serde_json::from_slice(&body)
            .with_context(|| format!("{path}: unexpected response"))?;
        streams
            .get(quality)
            .map(str::to_owned)
            .with_context(|| format!("console returned no {quality} stream"))
    }

    /// Stream URL for `quality`, creating it when it doesn't exist yet.
    pub async fn stream_url(&self, camera: &str, quality: &str) -> Result<String> {
        match self.streams(camera).await?.get(quality) {
            Some(url) => Ok(url.to_owned()),
            None => self.create_stream(camera, quality).await,
        }
    }

    /// Opens `/v1/subscribe/<topic>`.
    pub async fn subscribe(&self, topic: &str) -> Result<WsStream> {
        let mut url = self.base.clone();
        let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
        url.set_scheme(scheme)
            .map_err(|()| anyhow!("cannot build websocket URL"))?;
        let url = format!(
            "{}/v1/subscribe/{topic}",
            url.as_str().trim_end_matches('/')
        );
        let mut request = url.as_str().into_client_request()?;
        request.headers_mut().insert(
            "X-API-KEY",
            self.api_key
                .parse()
                .context("API key is not a valid header")?,
        );
        let connector = self.ws_connector();
        match tokio_tungstenite::connect_async_tls_with_config(request, None, true, Some(connector))
            .await
        {
            Ok((stream, _)) => Ok(stream),
            Err(tungstenite::Error::Http(response))
                if response.status() == StatusCode::UNAUTHORIZED =>
            {
                Err(AuthError.into())
            }
            Err(err) => {
                Err(anyhow::Error::new(Transport(Box::new(err)))
                    .context(format!("{topic} websocket")))
            }
        }
    }
}

/// `https://<host>/proxy/protect/integration`, or a user-given URL (path defaulting to the API path).
pub fn base_url(host: &str) -> Result<Url> {
    let explicit = host.contains("://");
    let mut url = if explicit {
        Url::parse(host)
    } else {
        Url::parse(&format!("https://{host}"))
    }
    .with_context(|| format!("invalid host {host:?}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("host URL must be http:// or https://");
    }
    if url.path() == "/" {
        url.set_path(API_PATH);
    }
    Ok(url)
}

/// `host[:port]` of an address: the console's identity for the keyring and session cache.
fn console_id(host: &str) -> Result<String> {
    let base = base_url(host)?;
    let hostname = base.host_str().context("config `host` has no hostname")?;
    Ok(match base.port() {
        Some(port) => format!("{hostname}:{port}"),
        None => hostname.to_owned(),
    })
}

/// Strips `?enableSrtp` and points the URL at the configured console, keeping its port.
pub fn fixup_stream_url(url: &str, hostname: &str) -> Result<String> {
    let mut url = Url::parse(url).with_context(|| format!("invalid stream URL {url:?}"))?;
    url.set_host(Some(hostname))
        .with_context(|| format!("cannot set stream host to {hostname}"))?;
    let query: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| k != "enableSrtp")
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    if query.is_empty() {
        url.set_query(None);
    } else {
        url.query_pairs_mut().clear().extend_pairs(query);
    }
    Ok(url.into())
}

fn api_error_message(body: &[u8]) -> String {
    #[derive(Deserialize)]
    struct ApiError {
        error: String,
    }
    serde_json::from_slice::<ApiError>(body)
        .map(|e| format!(": {}", e.error))
        .unwrap_or_default()
}

/// reqwest's Display omits the cause ("error sending request"); include the chain.
pub fn error_chain(err: &dyn std::error::Error) -> String {
    let mut message = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        let text = cause.to_string();
        if !message.contains(&text) {
            message.push_str(": ");
            message.push_str(&text);
        }
        source = cause.source();
    }
    message
}

/// A network or TLS failure. Displays its whole cause chain on one line and keeps the original
/// error so the failure can be classified.
#[derive(Debug)]
pub struct Transport(Box<dyn std::error::Error + Send + Sync>);

impl fmt::Display for Transport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&error_chain(self.0.as_ref()))
    }
}

impl std::error::Error for Transport {}

impl Transport {
    pub fn inner(&self) -> &(dyn std::error::Error + 'static) {
        self.0.as_ref()
    }
}

pub fn transport(err: reqwest::Error) -> anyhow::Error {
    Transport(Box::new(err)).into()
}

/// The certificate rejection behind a failed request, if that is why it failed.
pub fn rejection(err: &anyhow::Error) -> Option<&Rejection> {
    err.chain()
        .find_map(|cause| cause.downcast_ref::<Transport>())
        .and_then(|t| tls::rejection(t.inner()))
}

/// A non-success HTTP status from the Integration API.
#[derive(Debug)]
struct HttpError {
    path: String,
    status: StatusCode,
    message: String,
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: HTTP {}{}", self.path, self.status, self.message)
    }
}

impl std::error::Error for HttpError {}

/// Shown when the console's certificate isn't publicly trusted and no pin is configured.
pub const PIN_HINT: &str = "Pin your console's certificate: run sauron setup";
/// Longest offline message; the full errors go to the log.
const SUMMARY_MAX_CHARS: usize = 120;

/// What happened when trying one address, in words short enough for the bar.
#[derive(Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Timeout, refused, unreachable, DNS failure.
    NoAnswer,
    /// The pin didn't match: some other machine answers at this address.
    NotYourConsole,
    /// No pin, and webpki doesn't trust the certificate.
    Untrusted,
    Auth,
    Http(StatusCode),
    Other(String),
}

impl Outcome {
    pub fn of(err: &anyhow::Error) -> Self {
        if is_auth(err) {
            return Outcome::Auth;
        }
        for cause in err.chain() {
            if let Some(Transport(inner)) = cause.downcast_ref::<Transport>() {
                let inner: &(dyn std::error::Error + 'static) = inner.as_ref();
                return match (tls::rejection(inner), tls::find(inner)) {
                    (Some(Rejection::Mismatch { .. }), _) => Outcome::NotYourConsole,
                    (Some(Rejection::Untrusted { .. }), _) => Outcome::Untrusted,
                    (None, Some(_)) => Outcome::Other("TLS handshake failed".into()),
                    (None, None) => Outcome::NoAnswer,
                };
            }
            if let Some(http) = cause.downcast_ref::<HttpError>() {
                return Outcome::Http(http.status);
            }
            if cause.is::<serde_json::Error>() {
                return Outcome::Other("unexpected response".into());
            }
        }
        Outcome::Other(err.to_string())
    }
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Outcome::NoAnswer => f.write_str("no answer"),
            Outcome::NotYourConsole => f.write_str("not your console (certificate mismatch)"),
            Outcome::Untrusted => f.write_str("untrusted certificate"),
            Outcome::Auth => f.write_str("API key rejected"),
            Outcome::Http(status) => write!(f, "HTTP {}", status.as_u16()),
            Outcome::Other(message) => f.write_str(message),
        }
    }
}

/// One failed address.
pub struct Attempt {
    pub address: String,
    pub outcome: Outcome,
    pub error: anyhow::Error,
}

/// Why no address of the console could be used.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// The console rejected the API key.
    Auth,
    /// A certificate isn't trusted and there is no pin: setup is needed.
    Untrusted,
    Offline,
}

/// Every address failed; Display is the status message.
pub struct ConnectError {
    pub attempts: Vec<Attempt>,
}

impl ConnectError {
    pub fn failure(&self) -> Failure {
        let any = |outcome: Outcome| self.attempts.iter().any(|a| a.outcome == outcome);
        if any(Outcome::Auth) {
            Failure::Auth
        } else if any(Outcome::Untrusted) {
            Failure::Untrusted
        } else {
            Failure::Offline
        }
    }

    /// `a: no answer · b: not your console (certificate mismatch)`, at most about 120 characters.
    pub fn summary(&self) -> String {
        let clauses: Vec<String> = self
            .attempts
            .iter()
            .map(|a| format!("{}: {}", a.address, a.outcome))
            .collect();
        truncate(clauses.join(" · "), SUMMARY_MAX_CHARS)
    }
}

impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.failure() {
            Failure::Auth => fmt::Display::fmt(&AuthError, f),
            Failure::Untrusted => f.write_str(PIN_HINT),
            Failure::Offline => f.write_str(&self.summary()),
        }
    }
}

impl fmt::Debug for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl std::error::Error for ConnectError {}

/// `address: outcome` for a session that failed after connecting, short enough for the bar.
pub fn failure_message(address: &str, err: &anyhow::Error) -> String {
    truncate(
        format!("{address}: {}", Outcome::of(err)),
        SUMMARY_MAX_CHARS,
    )
}

/// Cuts `text` to `max` characters, ending with an ellipsis when shortened.
fn truncate(mut text: String, max: usize) -> String {
    if let Some((cut, _)) = text.char_indices().nth(max.saturating_sub(1))
        && text.chars().count() > max
    {
        text.truncate(cut);
        text.push('…');
    }
    text
}

/// Tries `host`, then each fallback: pinned TLS (3 s to connect), then `meta/info` with the key.
/// The first address that answers is the session's active address. A 401 stops the search.
pub async fn connect(config: &Config) -> Result<(Protect, String), ConnectError> {
    let mut attempts = Vec::new();
    for address in std::iter::once(&config.host).chain(&config.fallback_hosts) {
        let result = match Protect::new(config, address) {
            Ok(client) => client.version().await.map(|version| (client, version)),
            Err(err) => Err(err),
        };
        match result {
            Ok(connected) => return Ok(connected),
            Err(error) => {
                let outcome = Outcome::of(&error);
                let stop = outcome == Outcome::Auth;
                attempts.push(Attempt {
                    address: address.clone(),
                    outcome,
                    error,
                });
                if stop {
                    break;
                }
            }
        }
    }
    Err(ConnectError { attempts })
}

#[cfg(test)]
mod tests {
    use std::io;

    use rustls::pki_types::CertificateDer;
    use rustls::{CertificateError, OtherError};

    use super::*;

    #[test]
    fn base_url_defaults_to_https_and_the_api_path() {
        let url = |host: &str| base_url(host).unwrap().to_string();
        assert_eq!(
            url("192.0.2.10"),
            "https://192.0.2.10/proxy/protect/integration"
        );
        assert_eq!(
            url("unifi.example:8443"),
            "https://unifi.example:8443/proxy/protect/integration"
        );
        // An explicit URL keeps its scheme, port and any path.
        assert_eq!(
            url("http://192.0.2.10:8080"),
            "http://192.0.2.10:8080/proxy/protect/integration"
        );
        assert_eq!(
            url("https://192.0.2.10/custom/api"),
            "https://192.0.2.10/custom/api"
        );
    }

    #[test]
    fn base_url_rejects_other_schemes_and_garbage() {
        assert!(
            format!("{:#}", base_url("ftp://192.0.2.10").unwrap_err())
                .contains("http:// or https://")
        );
        assert!(format!("{:#}", base_url("not a host").unwrap_err()).contains("invalid host"));
    }

    #[test]
    fn console_id_is_host_and_explicit_port() {
        assert_eq!(console_id("192.0.2.10").unwrap(), "192.0.2.10");
        assert_eq!(
            console_id("https://unifi.example:7443/proxy/protect/integration").unwrap(),
            "unifi.example:7443"
        );
    }

    #[test]
    fn stream_url_loses_srtp_and_points_at_the_console() {
        assert_eq!(
            fixup_stream_url("rtsps://10.0.0.5:7441/aBcToKeN123?enableSrtp", "192.0.2.10").unwrap(),
            "rtsps://192.0.2.10:7441/aBcToKeN123"
        );
        assert_eq!(
            fixup_stream_url(
                "rtsps://10.0.0.5:7441/tok?enableSrtp&foo=bar",
                "unifi.example"
            )
            .unwrap(),
            "rtsps://unifi.example:7441/tok?foo=bar"
        );
        assert_eq!(
            fixup_stream_url("rtsps://10.0.0.5/tok", "192.0.2.10").unwrap(),
            "rtsps://192.0.2.10/tok"
        );
    }

    fn attempts(outcomes: Vec<(&str, Outcome)>) -> ConnectError {
        let attempts = outcomes
            .into_iter()
            .map(|(address, outcome)| Attempt {
                address: address.into(),
                outcome,
                error: anyhow!("failed"),
            })
            .collect();
        ConnectError { attempts }
    }

    #[test]
    fn offline_message_joins_every_address() {
        let err = attempts(vec![
            ("192.0.2.10", Outcome::NoAnswer),
            ("unifi.example", Outcome::NotYourConsole),
        ]);
        assert!(err.failure() == Failure::Offline);
        assert_eq!(
            err.to_string(),
            "192.0.2.10: no answer · unifi.example: not your console (certificate mismatch)"
        );
    }

    #[test]
    fn auth_beats_untrusted_which_beats_offline() {
        let auth = attempts(vec![
            ("192.0.2.10", Outcome::Untrusted),
            ("198.51.100.7", Outcome::NoAnswer),
            ("unifi.example", Outcome::Auth),
        ]);
        assert!(auth.failure() == Failure::Auth);
        assert_eq!(auth.to_string(), "API key rejected (401)");

        let untrusted = attempts(vec![
            ("192.0.2.10", Outcome::NoAnswer),
            ("unifi.example", Outcome::Untrusted),
        ]);
        assert!(untrusted.failure() == Failure::Untrusted);
        assert_eq!(untrusted.to_string(), PIN_HINT);
    }

    #[test]
    fn offline_message_is_cut_to_120_characters() {
        let long = attempts(
            (0..6)
                .map(|_| ("very-long-hostname.unifi.example", Outcome::NoAnswer))
                .collect(),
        );
        let message = long.to_string();
        assert_eq!(message.chars().count(), SUMMARY_MAX_CHARS);
        assert!(
            message.starts_with("very-long-hostname.unifi.example: no answer · ")
                && message.ends_with('…')
        );

        assert_eq!(truncate("é".repeat(120), 120), "é".repeat(120));
        assert_eq!(
            truncate("é".repeat(121), 120),
            format!("{}…", "é".repeat(119))
        );
    }

    fn tls_failure(tls: rustls::Error) -> anyhow::Error {
        // As reqwest reports it: the rustls error inside an I/O error inside the transport error.
        anyhow::Error::new(Transport(Box::new(io::Error::other(tls)))).context("GET /v1/meta/info")
    }

    fn rejected(rejection: Rejection) -> anyhow::Error {
        tls_failure(rustls::Error::InvalidCertificate(CertificateError::Other(
            OtherError(Arc::new(rejection)),
        )))
    }

    #[test]
    fn outcome_classifies_certificate_failures() {
        let leaf = CertificateDer::from(b"leaf".to_vec());
        let mismatch = rejected(Rejection::Mismatch { leaf: leaf.clone() });
        assert!(Outcome::of(&mismatch) == Outcome::NotYourConsole);
        assert!(matches!(
            rejection(&mismatch),
            Some(Rejection::Mismatch { .. })
        ));

        let reason = rustls::Error::InvalidCertificate(CertificateError::UnknownIssuer);
        assert!(
            Outcome::of(&rejected(Rejection::Untrusted { leaf, reason })) == Outcome::Untrusted
        );

        let handshake = tls_failure(rustls::Error::General("bad record".into()));
        assert!(Outcome::of(&handshake) == Outcome::Other("TLS handshake failed".into()));
        assert!(rejection(&handshake).is_none());
    }

    #[test]
    fn outcome_classifies_other_failures() {
        let refused = anyhow::Error::new(Transport(Box::new(io::Error::from(
            io::ErrorKind::ConnectionRefused,
        ))));
        assert!(Outcome::of(&refused) == Outcome::NoAnswer);

        let auth = anyhow::Error::new(AuthError).context("events websocket");
        assert!(Outcome::of(&auth) == Outcome::Auth);

        let http = HttpError {
            path: "/v1/cameras".into(),
            status: StatusCode::NOT_FOUND,
            message: String::new(),
        };
        let http = anyhow::Error::new(http);
        assert!(Outcome::of(&http) == Outcome::Http(StatusCode::NOT_FOUND));
        assert_eq!(failure_message("192.0.2.10", &http), "192.0.2.10: HTTP 404");

        let json =
            anyhow::Error::new(serde_json::from_str::<u8>("x").unwrap_err()).context("/v1/cameras");
        assert!(Outcome::of(&json) == Outcome::Other("unexpected response".into()));
    }

    #[test]
    fn camera_kinds_follow_feature_flags_and_model() {
        let camera: Camera = serde_json::from_str(
            r#"{"id":"cam","type":"UVC G4 Doorbell Pro","featureFlags":
                {"smartDetectTypes":["person","package"],"smartDetectAudioTypes":["alrmSmoke"]}}"#,
        )
        .unwrap();
        assert_eq!(
            camera.kinds(),
            ["person", "package", "audio", "motion", "ring"]
        );

        let plain: Camera = serde_json::from_str(
            r#"{"id":"cam","type":"UVC G3","featureFlags":{"smartDetectAudioTypes":[]}}"#,
        )
        .unwrap();
        assert_eq!(plain.kinds(), ["motion"]);
    }

    #[test]
    fn display_name_falls_back_to_model_then_id() {
        let camera = |name: Option<&str>, model: Option<&str>| Camera {
            id: "cam-id".into(),
            name: name.map(str::to_owned),
            model: model.map(str::to_owned),
            ..Camera::default()
        };
        assert_eq!(camera(Some("Porch"), Some("G4")).display_name(), "Porch");
        assert_eq!(camera(Some("  "), Some("G4")).display_name(), "G4");
        assert_eq!(camera(None, None).display_name(), "cam-id");
    }

    #[test]
    fn device_ids_are_one_or_many() {
        let one: DeviceItem = serde_json::from_str(r#"{"id":"a","modelKey":"camera"}"#).unwrap();
        let many: DeviceItem =
            serde_json::from_str(r#"{"id":["a","b"],"modelKey":"camera"}"#).unwrap();
        assert_eq!(one.id.as_slice(), ["a"]);
        assert_eq!(many.id.as_slice(), ["a", "b"]);
    }
}
