//! Live view: player spawning and window focus.

use std::path::Path;
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use tokio::process::{Child, Command};

/// Spawns the configured player for `url` in its own process group with null stdio.
pub fn spawn_player(player: &[String], camera_name: &str, url: &str) -> Result<Child> {
    let (program, args) = player.split_first().context("`player` is empty")?;
    let mut command = Command::new(program);
    command.args(args);
    if Path::new(program).file_name().is_some_and(|name| name == "mpv") {
        command.arg(format!("--title=Sauron · {camera_name}"));
    }
    command
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .with_context(|| format!("cannot start player `{program}`"))
}

/// Focuses the window owned by `pid` (Lua dispatcher on Hyprland ≥ 0.55, legacy syntax otherwise).
pub async fn focus(pid: u32) -> Result<()> {
    let lua = format!("hl.dsp.focus({{ window = \"pid:{pid}\" }})");
    let reply = hyprctl(&["dispatch", &lua]).await?;
    if reply == "ok" {
        return Ok(());
    }
    if hyprctl(&["dispatch", "focuswindow", &format!("pid:{pid}")]).await? == "ok" {
        return Ok(());
    }
    bail!("cannot focus player window: {reply}")
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
