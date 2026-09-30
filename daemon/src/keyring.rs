//! The instant-live password, kept in the desktop keyring through `secret-tool`.

use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// A locked keyring may prompt; don't wait on it forever.
const TIMEOUT: Duration = Duration::from_secs(5);

fn attributes<'a>(console: &'a str, username: &'a str) -> [&'a str; 6] {
    [
        "application",
        "sauron",
        "host",
        console,
        "username",
        username,
    ]
}

/// The stored password, or `None` if the keyring is unavailable or has no entry.
pub async fn lookup(console: &str, username: &str) -> Option<String> {
    let output = Command::new("secret-tool")
        .arg("lookup")
        .args(attributes(console, username))
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(TIMEOUT, output).await.ok()?.ok()?;
    let password = String::from_utf8(output.stdout).ok()?;
    let password = password.strip_suffix('\n').unwrap_or(&password);
    (output.status.success() && !password.is_empty()).then(|| password.to_owned())
}

pub async fn store(console: &str, username: &str, password: &str) -> Result<()> {
    let mut child = Command::new("secret-tool")
        .arg("store")
        .arg(format!("--label=Sauron · {console}"))
        .args(attributes(console, username))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("cannot run secret-tool")?;
    let mut stdin = child.stdin.take().context("secret-tool has no stdin")?;
    stdin
        .write_all(password.as_bytes())
        .await
        .context("cannot pass the password to secret-tool")?;
    drop(stdin);
    let output = tokio::time::timeout(TIMEOUT, child.wait_with_output())
        .await
        .context("secret-tool timed out (is the keyring unlocked?)")??;
    if !output.status.success() {
        bail!(
            "secret-tool store failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}
