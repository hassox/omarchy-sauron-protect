use std::fmt;
use std::io::ErrorKind;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result};
use serde::Deserialize;
use tokio::io::AsyncWriteExt;

const DEFAULT_PLAYER: [&str; 5] = ["mpv", "--profile=low-latency", "--untimed", "--no-cache", "--force-window=immediate"];

/// Validated configuration with the API key already resolved.
#[derive(Clone)]
pub struct Config {
    pub host: String,
    pub api_key: String,
    /// Local UniFi OS user for instant live; empty when instant live is not set up.
    pub username: String,
    pub verify_tls: bool,
    pub live_quality: String,
    pub player: Vec<String>,
}

/// The config file as written, before validation.
#[derive(Deserialize, Clone)]
#[serde(default)]
pub struct Settings {
    pub host: String,
    pub api_key: String,
    pub api_key_command: String,
    pub username: String,
    pub verify_tls: bool,
    pub live_quality: String,
    pub player: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            host: String::new(),
            api_key: String::new(),
            api_key_command: String::new(),
            username: String::new(),
            verify_tls: false,
            live_quality: "high".into(),
            player: DEFAULT_PLAYER.map(String::from).to_vec(),
        }
    }
}

/// Why the config cannot be used; the Display text is shown to humans as-is.
#[derive(Debug)]
pub enum LoadError {
    /// No config file, or no API key.
    NotSetUp,
    Invalid(String),
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoadError::NotSetUp => f.write_str("Not set up yet: run sauron setup"),
            LoadError::Invalid(message) => write!(f, "Config error: {message}"),
        }
    }
}

impl std::error::Error for LoadError {}

fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key).filter(|v| !v.is_empty()).map(PathBuf::from)
}

pub fn config_path() -> PathBuf {
    if let Some(path) = env_path("SAURON_CONFIG") {
        return path;
    }
    env_path("XDG_CONFIG_HOME")
        .unwrap_or_else(|| env_path("HOME").unwrap_or_else(|| PathBuf::from("/")).join(".config"))
        .join("sauron/config.toml")
}

pub fn runtime_dir() -> PathBuf {
    env_path("SAURON_RUNTIME_DIR")
        .or_else(|| env_path("XDG_RUNTIME_DIR").map(|dir| dir.join("sauron")))
        // SAFETY: getuid has no preconditions and cannot fail.
        .unwrap_or_else(|| PathBuf::from(format!("/tmp/sauron-{}", unsafe { libc::getuid() })))
}

/// Reads the config file; `Ok(None)` when it doesn't exist.
pub async fn read_settings(path: &Path) -> Result<Option<Settings>, LoadError> {
    let text = match tokio::fs::read_to_string(path).await {
        Ok(text) => text,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(LoadError::Invalid(format!("cannot read the file: {err}"))),
    };
    toml::from_str(&text).map(Some).map_err(|err| LoadError::Invalid(one_line(&err, &text)))
}

/// "line 3: invalid string" instead of toml's multi-line snippet.
fn one_line(err: &toml::de::Error, text: &str) -> String {
    let message = err.message().trim().replace('\n', " ");
    match err.span() {
        Some(span) => {
            let line = text.as_bytes()[..span.start.min(text.len())].iter().filter(|&&b| b == b'\n').count() + 1;
            format!("line {line}: {message}")
        }
        None => message,
    }
}

/// Loads and validates the config.
pub async fn load(path: &Path) -> Result<Config, LoadError> {
    read_settings(path).await?.ok_or(LoadError::NotSetUp)?.validate().await
}

impl Settings {
    /// Checks the values and resolves the API key.
    pub async fn validate(self) -> Result<Config, LoadError> {
        let invalid = |message: String| Err(LoadError::Invalid(message));
        let host = self.host.trim().trim_end_matches('/').to_owned();
        if host.is_empty() {
            return invalid("`host` is empty".into());
        }
        if !matches!(self.live_quality.as_str(), "high" | "medium" | "low") {
            return invalid(format!("`live_quality` must be high, medium or low, not {:?}", self.live_quality));
        }
        if self.player.first().is_none_or(|p| p.is_empty()) {
            return invalid("`player` must be a non-empty list".into());
        }
        let api_key = match (self.api_key.trim(), self.api_key_command.trim()) {
            ("", "") => return Err(LoadError::NotSetUp),
            (key, "") => key.to_owned(),
            ("", command) => {
                run_key_command(command).await.map_err(|err| LoadError::Invalid(format!("{err:#}")))?
            }
            _ => return invalid("set only one of `api_key` and `api_key_command`".into()),
        };
        Ok(Config {
            host,
            api_key,
            username: self.username.trim().to_owned(),
            verify_tls: self.verify_tls,
            live_quality: self.live_quality,
            player: self.player,
        })
    }
}

async fn run_key_command(command: &str) -> Result<String> {
    let output = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .output()
        .await
        .context("cannot run `api_key_command`")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        let separator = if stderr.is_empty() { "" } else { ": " };
        anyhow::bail!("`api_key_command` failed ({}){separator}{stderr}", output.status);
    }
    let key = String::from_utf8(output.stdout).context("`api_key_command` printed non-UTF-8 output")?;
    let key = key.trim();
    if key.is_empty() {
        anyhow::bail!("`api_key_command` printed nothing");
    }
    Ok(key.to_owned())
}

/// The commented config file with `settings` filled in.
fn render(settings: &Settings) -> String {
    let string = |s: &str| toml::Value::String(s.to_owned()).to_string();
    let player = toml::Value::Array(settings.player.iter().map(|arg| toml::Value::String(arg.clone())).collect());
    let key_command = match settings.api_key_command.trim() {
        "" => "# api_key_command = \"\"".to_owned(),
        command => format!("api_key_command = {}", string(command)),
    };
    format!(
        "# UniFi console address: IP or hostname (https assumed). A full URL (http://host:port) is accepted for testing.
host = {host}
# Protect › Settings › Control Plane › Integrations › Create API key
api_key = {api_key}
# Alternative to api_key: a command whose stdout is the key, e.g. \"secret-tool lookup service sauron\"
{key_command}
# Local UniFi OS user for instant live video; its password lives in the keyring. Empty = RTSPS only.
username = {username}
# UniFi consoles ship self-signed certificates; set true only if yours has a valid one.
verify_tls = {verify_tls}
# Live view quality: high | medium | low
live_quality = {live_quality}
# Player argv; \"--title=<Sauron · camera>\" (mpv only) and the stream URL (or \"-\" for instant live) are appended.
player = {player}
",
        host = string(settings.host.trim()),
        api_key = string(settings.api_key.trim()),
        username = string(settings.username.trim()),
        verify_tls = settings.verify_tls,
        live_quality = string(&settings.live_quality),
    )
}

/// Atomically writes the config (mode 0600), creating the parent directory.
pub async fn save(path: &Path, settings: &Settings) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .await
        .with_context(|| format!("cannot create {}", tmp.display()))?;
    file.write_all(render(settings).as_bytes()).await?;
    file.sync_all().await?;
    drop(file);
    tokio::fs::rename(&tmp, path).await.with_context(|| format!("cannot write {}", path.display()))
}

/// The IPv4 default gateway from `/proc/net/route`, if any.
pub async fn default_gateway() -> Option<Ipv4Addr> {
    let table = tokio::fs::read_to_string("/proc/net/route").await.ok()?;
    table.lines().skip(1).find_map(|line| {
        let mut fields = line.split_whitespace();
        let (_iface, destination, gateway) = (fields.next()?, fields.next()?, fields.next()?);
        if destination != "00000000" {
            return None;
        }
        let gateway = u32::from_str_radix(gateway, 16).ok().filter(|&g| g != 0)?;
        Some(Ipv4Addr::from(gateway.to_le_bytes()))
    })
}
