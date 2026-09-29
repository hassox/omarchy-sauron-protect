use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use tokio::io::AsyncWriteExt;

const TEMPLATE: &str = r#"# UniFi console address: IP or hostname (https assumed). A full URL (http://host:port) is accepted for testing.
host = "192.168.1.1"
# Protect → Settings → Control Plane → Integrations → Create API key
api_key = ""
# Alternative to api_key: a command whose stdout is the key, e.g. "secret-tool lookup service sauron"
# api_key_command = ""
# UniFi consoles ship self-signed certificates; set true only if yours has a valid one.
verify_tls = false
# Live view quality: high | medium | low
live_quality = "high"
# Player argv; "--title=<Sauron · camera>" (mpv only) and the stream URL are appended.
player = ["mpv", "--profile=low-latency", "--untimed", "--no-cache", "--force-window=immediate"]
"#;

/// Validated configuration with the API key already resolved.
#[derive(Clone)]
pub struct Config {
    pub host: String,
    pub api_key: String,
    pub verify_tls: bool,
    pub live_quality: String,
    pub player: Vec<String>,
}

#[derive(Deserialize)]
#[serde(default)]
struct RawConfig {
    host: String,
    api_key: String,
    api_key_command: String,
    verify_tls: bool,
    live_quality: String,
    player: Vec<String>,
}

impl Default for RawConfig {
    fn default() -> Self {
        Self {
            host: String::new(),
            api_key: String::new(),
            api_key_command: String::new(),
            verify_tls: false,
            live_quality: "high".into(),
            player: ["mpv", "--profile=low-latency", "--untimed", "--no-cache", "--force-window=immediate"]
                .map(String::from)
                .to_vec(),
        }
    }
}

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

/// Loads and validates the config; every error names the config path.
pub async fn load(path: &Path) -> Result<Config> {
    load_inner(path).await.with_context(|| format!("config {}", path.display()))
}

async fn load_inner(path: &Path) -> Result<Config> {
    let text = match tokio::fs::read_to_string(path).await {
        Ok(text) => text,
        Err(err) if err.kind() == ErrorKind::NotFound => {
            bail!("file not found (run `sauron config` to create it)")
        }
        Err(err) => return Err(err).context("cannot read file"),
    };
    let raw: RawConfig = toml::from_str(&text).context("invalid TOML")?;

    let host = raw.host.trim().trim_end_matches('/').to_owned();
    if host.is_empty() {
        bail!("`host` is empty");
    }
    if !matches!(raw.live_quality.as_str(), "high" | "medium" | "low") {
        bail!("`live_quality` must be high, medium or low (got {:?})", raw.live_quality);
    }
    if raw.player.first().is_none_or(|p| p.is_empty()) {
        bail!("`player` must be a non-empty argv array");
    }
    let api_key = match (raw.api_key.trim(), raw.api_key_command.trim()) {
        ("", "") => bail!("no API key: set `api_key` or `api_key_command`"),
        (_, "") => raw.api_key.trim().to_owned(),
        ("", command) => run_key_command(command).await?,
        _ => bail!("set only one of `api_key` and `api_key_command`"),
    };
    Ok(Config { host, api_key, verify_tls: raw.verify_tls, live_quality: raw.live_quality, player: raw.player })
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
        bail!("`api_key_command` failed ({}){separator}{stderr}", output.status);
    }
    let key = String::from_utf8(output.stdout).context("`api_key_command` printed non-UTF-8 output")?;
    let key = key.trim();
    if key.is_empty() {
        bail!("`api_key_command` printed nothing");
    }
    Ok(key.to_owned())
}

/// Writes the template (0600, parent dirs created) unless the file exists. Returns true if written.
pub async fn write_template(path: &Path) -> Result<bool> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    let file = tokio::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path).await;
    let mut file = match file {
        Ok(file) => file,
        Err(err) if err.kind() == ErrorKind::AlreadyExists => return Ok(false),
        Err(err) => return Err(err).with_context(|| format!("cannot create {}", path.display())),
    };
    file.write_all(TEMPLATE.as_bytes()).await?;
    file.flush().await?;
    Ok(true)
}
