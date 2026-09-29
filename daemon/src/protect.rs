//! UniFi Protect Integration API client: REST calls and websocket subscriptions.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use reqwest::{Method, StatusCode, Url};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use tokio::net::TcpStream;
use tokio::sync::Semaphore;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{self, Bytes};
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};

use crate::config::Config;

const API_PATH: &str = "/proxy/protect/integration";
const MAX_CONCURRENT_REQUESTS: usize = 4;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
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
        self.feature_flags.as_ref().and_then(|f| f.support_full_hd_snapshot) == Some(true)
    }

    /// Detection kinds this camera can produce.
    pub fn kinds(&self) -> Vec<String> {
        let flags = self.feature_flags.as_ref();
        let mut kinds: Vec<String> =
            flags.and_then(|f| f.smart_detect_types.clone()).unwrap_or_default();
        if flags.and_then(|f| f.smart_detect_audio_types.as_ref()).is_some_and(|a| !a.is_empty()) {
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

pub struct Protect {
    http: reqwest::Client,
    base: Url,
    api_key: String,
    hostname: String,
    ws_tls: Arc<ClientConfig>,
    permits: Semaphore,
}

impl Protect {
    /// Client for the configured console; an empty API key sends no key (setup's console probe).
    pub fn new(config: &Config) -> Result<Self> {
        let base = base_url(&config.host)?;
        let hostname = base.host_str().context("config `host` has no hostname")?.to_owned();
        let tls = tls_config(config.verify_tls)?;

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
            hostname,
            ws_tls: Arc::new(ws_tls),
            permits: Semaphore::new(MAX_CONCURRENT_REQUESTS),
        })
    }

    /// Hostname of the configured console, used to rewrite stream URLs.
    pub fn hostname(&self) -> &str {
        &self.hostname
    }

    /// `host[:port]` of the console: the keyring and session-cache identity.
    pub fn console_id(&self) -> String {
        match self.base.port() {
            Some(port) => format!("{}:{port}", self.hostname),
            None => self.hostname.clone(),
        }
    }

    /// `scheme://host[:port]` of the console, for the UniFi OS (non-integration) endpoints.
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
        let response =
            self.http.get(self.url("/v1/meta/info")).send().await.map_err(|e| anyhow!(error_chain(&e)))?;
        Ok(response.status())
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
        let response = request.send().await.map_err(|e| anyhow!(error_chain(&e)))?;
        let status = response.status();
        let body = response.bytes().await.map_err(|e| anyhow!(error_chain(&e)))?;
        if status == StatusCode::UNAUTHORIZED {
            return Err(AuthError.into());
        }
        if !status.is_success() {
            bail!("{path}: HTTP {status}{}", api_error_message(&body));
        }
        Ok(body)
    }

    async fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let body = self.request(Method::GET, path, None, REQUEST_TIMEOUT).await?;
        serde_json::from_slice(&body).with_context(|| format!("{path}: unexpected response"))
    }

    /// Protect application version; also validates the key.
    pub async fn version(&self) -> Result<String> {
        Ok(self.get_json::<MetaInfo>("/v1/meta/info").await?.application_version)
    }

    pub async fn cameras(&self) -> Result<Vec<Camera>> {
        self.get_json("/v1/cameras").await
    }

    pub async fn snapshot(&self, camera: &str, high_quality: bool) -> Result<Bytes> {
        let path = format!("/v1/cameras/{camera}/snapshot?highQuality={high_quality}");
        self.request(Method::GET, &path, None, SNAPSHOT_TIMEOUT).await
    }

    async fn streams(&self, camera: &str) -> Result<Streams> {
        self.get_json(&format!("/v1/cameras/{camera}/rtsps-stream")).await
    }

    /// Creates (or returns the existing) stream of `quality`.
    async fn create_stream(&self, camera: &str, quality: &str) -> Result<String> {
        let path = format!("/v1/cameras/{camera}/rtsps-stream");
        let body = serde_json::json!({ "qualities": [quality] });
        let body = self.request(Method::POST, &path, Some(&body), REQUEST_TIMEOUT).await?;
        let streams: Streams = serde_json::from_slice(&body).with_context(|| format!("{path}: unexpected response"))?;
        streams.get(quality).map(str::to_owned).with_context(|| format!("console returned no {quality} stream"))
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
        url.set_scheme(scheme).map_err(|()| anyhow!("cannot build websocket URL"))?;
        let url = format!("{}/v1/subscribe/{topic}", url.as_str().trim_end_matches('/'));
        let mut request = url.as_str().into_client_request()?;
        request.headers_mut().insert("X-API-KEY", self.api_key.parse().context("API key is not a valid header")?);
        let connector = self.ws_connector();
        match tokio_tungstenite::connect_async_tls_with_config(request, None, true, Some(connector)).await {
            Ok((stream, _)) => Ok(stream),
            Err(tungstenite::Error::Http(response)) if response.status() == StatusCode::UNAUTHORIZED => {
                Err(AuthError.into())
            }
            Err(err) => Err(anyhow!("{topic} websocket: {err}")),
        }
    }
}

/// `https://<host>/proxy/protect/integration`, or a user-given URL (path defaulting to the API path).
fn base_url(host: &str) -> Result<Url> {
    let explicit = host.contains("://");
    let mut url = if explicit { Url::parse(host) } else { Url::parse(&format!("https://{host}")) }
        .with_context(|| format!("invalid host {host:?}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("host URL must be http:// or https://");
    }
    if url.path() == "/" {
        url.set_path(API_PATH);
    }
    Ok(url)
}

/// Strips `?enableSrtp` and points the URL at the configured console, keeping its port.
pub fn fixup_stream_url(url: &str, hostname: &str) -> Result<String> {
    let mut url = Url::parse(url).with_context(|| format!("invalid stream URL {url:?}"))?;
    url.set_host(Some(hostname)).with_context(|| format!("cannot set stream host to {hostname}"))?;
    let query: Vec<(String, String)> =
        url.query_pairs().filter(|(k, _)| k != "enableSrtp").map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
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
    serde_json::from_slice::<ApiError>(body).map(|e| format!(": {}", e.error)).unwrap_or_default()
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

/// Shared rustls config (ring provider, no ALPN): webpki roots, or certificate checks disabled.
fn tls_config(verify: bool) -> Result<ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = ClientConfig::builder_with_provider(provider.clone()).with_safe_default_protocol_versions()?;
    Ok(if verify {
        let roots = RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
        builder.with_root_certificates(roots).with_no_client_auth()
    } else {
        builder.dangerous().with_custom_certificate_verifier(Arc::new(NoCertVerification(provider))).with_no_client_auth()
    })
}

/// Accepts any certificate chain (consoles are self-signed) but still verifies handshake signatures.
#[derive(Debug)]
struct NoCertVerification(Arc<CryptoProvider>);

impl ServerCertVerifier for NoCertVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}
