//! Hyprland window lookup and focus through `hyprctl`.

use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use tokio::process::Command;
use tokio::time::{Instant, sleep};

const MAP_POLL: Duration = Duration::from_millis(50);
const MAP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct Client {
    pub address: String,
    pub pid: i64,
    pub title: String,
}

pub async fn clients() -> Result<Vec<Client>> {
    let output = Command::new("hyprctl")
        .args(["clients", "-j"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .await
        .context("cannot run hyprctl")?;
    if !output.status.success() {
        bail!("hyprctl clients failed ({})", output.status);
    }
    serde_json::from_slice(&output.stdout).context("unexpected `hyprctl clients -j` output")
}

/// Focuses the window matching a Hyprland window selector such as `pid:123` or `address:0x…`
/// (Lua dispatcher on Hyprland ≥ 0.55, legacy syntax otherwise).
pub async fn focus(selector: &str) -> Result<()> {
    let reply = hyprctl(&[
        "dispatch",
        &format!("hl.dsp.focus({{ window = \"{selector}\" }})"),
    ])
    .await?;
    if reply == "ok" {
        return Ok(());
    }
    if hyprctl(&["dispatch", "focuswindow", selector]).await? == "ok" {
        return Ok(());
    }
    bail!("cannot focus window {selector}: {reply}")
}

/// Waits for a window owned by `pid` to map, then focuses it. Hyprland leaves a new window
/// unfocused while a layer-shell surface (the bar panel, a notification) holds the keyboard.
pub async fn focus_when_mapped(pid: u32) -> Result<()> {
    let deadline = Instant::now() + MAP_TIMEOUT;
    loop {
        if clients().await?.iter().any(|c| c.pid == i64::from(pid)) {
            return focus(&format!("pid:{pid}")).await;
        }
        if Instant::now() >= deadline {
            bail!(
                "no window appeared for pid {pid} within {}s",
                MAP_TIMEOUT.as_secs()
            );
        }
        sleep(MAP_POLL).await;
    }
}

async fn hyprctl(args: &[&str]) -> Result<String> {
    let output = Command::new("hyprctl")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await
        .context("cannot run hyprctl")?;
    let mut reply = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if reply.is_empty() {
        reply = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    }
    Ok(reply)
}
