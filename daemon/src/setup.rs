//! `sauron setup`: a short interactive wizard, drawn with `gum` like Omarchy's own.

use std::fmt;
use std::io::IsTerminal;
use std::process::{ExitCode, Stdio};

use anyhow::{Context, Result};
use reqwest::StatusCode;
use tokio::process::Command;

use crate::config::{self, Config, LoadError, Settings};
use crate::keyring;
use crate::private_api::{self, LoginError, PrivateApi};
use crate::protect::{self, Camera, Protect};

const KEY_HINT: &str = "Protect › Settings › Control Plane › Integrations › Create API key";
const INSTANT_HINT: &str = "Instant live video needs a local UniFi user: UniFi OS › Admins & Users › Add, \
                            Restrict to local access only, Protect: View Only.";

/// The user pressed Ctrl-C or Esc in a prompt.
#[derive(Debug)]
struct Cancelled;

impl fmt::Display for Cancelled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("setup cancelled")
    }
}

impl std::error::Error for Cancelled {}

pub async fn run() -> ExitCode {
    if !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
        eprintln!("sauron setup needs a terminal");
        return ExitCode::from(2);
    }
    match wizard().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) if err.is::<Cancelled>() => {
            eprintln!("sauron: setup cancelled; nothing saved");
            ExitCode::from(130)
        }
        Err(err) => {
            eprintln!("sauron: {err:#}");
            ExitCode::FAILURE
        }
    }
}

async fn wizard() -> Result<()> {
    let path = config::config_path();
    let mut settings = match config::read_settings(&path).await {
        Ok(settings) => settings.unwrap_or_default(),
        Err(err) => {
            warn(&format!("{err}; starting from defaults"));
            Settings::default()
        }
    };
    gum(&["style", "--bold", "--padding", "0 1", "Sauron setup"], Stdio::inherit()).await?;

    settings.host = ask_host(&settings).await?;
    let (client, cameras) = ask_api_key(&mut settings).await?;
    ask_instant_live(&mut settings, &client, &cameras).await?;

    config::save(&path, &settings).await?;
    println!("Saved {}. The eye opens in a few seconds.", path.display());
    Ok(())
}

/// Step 1: the console address, probed without a key (a Protect console answers 401).
async fn ask_host(settings: &Settings) -> Result<String> {
    let mut default = settings.host.trim().to_owned();
    if default.is_empty() {
        default = config::default_gateway().await.map(|ip| ip.to_string()).unwrap_or_default();
    }
    loop {
        let host = input("Console address", &default, "IP or hostname", false).await?;
        if host.is_empty() {
            continue;
        }
        default.clone_from(&host);
        // No key and no certificate check: any UniFi console answers 401 here.
        let probe = Config {
            host: host.clone(),
            api_key: String::new(),
            username: String::new(),
            verify_tls: false,
            live_quality: String::new(),
            player: Vec::new(),
        };
        let client = match Protect::new(&probe) {
            Ok(client) => client,
            Err(err) => {
                fail(&format!("{err:#}"));
                continue;
            }
        };
        match spin("Looking for the console…", client.probe()).await {
            Ok(StatusCode::UNAUTHORIZED) => {
                done("UniFi console found");
                return Ok(host);
            }
            Ok(status) => {
                warn(&format!("{host} answered HTTP {status}; it doesn't look like a UniFi console"));
                if confirm("Continue anyway?", false).await? {
                    return Ok(host);
                }
            }
            Err(err) => fail(&format!("Cannot reach {host}: {err:#}")),
        }
    }
}

/// Step 2: the Integration API key, verified with `meta/info` and the camera list.
async fn ask_api_key(settings: &mut Settings) -> Result<(Protect, Vec<Camera>)> {
    println!("{KEY_HINT}");
    let has_key = !settings.api_key.trim().is_empty() || !settings.api_key_command.trim().is_empty();
    let placeholder = if has_key { "Enter keeps the current key" } else { "paste the key" };
    loop {
        let key = input("API key", "", placeholder, true).await?;
        let mut candidate = settings.clone();
        if !key.is_empty() {
            candidate.api_key = key;
            candidate.api_key_command.clear();
        } else if !has_key {
            fail("An API key is required");
            continue;
        }
        let config = match candidate.clone().validate().await {
            Ok(config) => config,
            Err(LoadError::NotSetUp) => {
                fail("An API key is required");
                continue;
            }
            Err(err) => {
                fail(&err.to_string());
                continue;
            }
        };
        let client = Protect::new(&config)?;
        let verified = spin("Checking the API key…", async {
            let version = client.version().await?;
            let cameras = client.cameras().await?;
            anyhow::Ok((version, cameras))
        })
        .await;
        match verified {
            Ok((version, cameras)) => {
                done(&format!("Protect {version} · {} cameras", cameras.len()));
                *settings = candidate;
                return Ok((client, cameras));
            }
            Err(err) if protect::is_auth(&err) => fail("The console rejected that API key"),
            Err(err) => fail(&format!("{err:#}")),
        }
    }
}

/// Step 3 (optional): a local UniFi OS user for the private livestream; password to the keyring.
async fn ask_instant_live(settings: &mut Settings, client: &Protect, cameras: &[Camera]) -> Result<()> {
    println!("{INSTANT_HINT}");
    if !confirm("Set up instant live?", !settings.username.trim().is_empty()).await? {
        settings.username.clear();
        return Ok(());
    }
    let camera = cameras.iter().find(|c| c.online()).or(cameras.first());
    let channel = private_api::channel(&settings.live_quality);
    let mut username = settings.username.trim().to_owned();
    loop {
        username = input("UniFi username", &username, "local-only user", false).await?;
        let password = input("Password", "", "", true).await?;
        if username.is_empty() || password.is_empty() {
            fail("Both the username and the password are needed");
            continue;
        }
        let mut api = PrivateApi::new(client, &username, password.clone());
        let verified = spin("Logging in…", async {
            api.login().await?;
            if let Some(camera) = camera {
                api.livestream_url(&camera.id, channel).await?;
            }
            anyhow::Ok(())
        })
        .await;
        match verified {
            Ok(()) => {
                keyring::store(&client.console_id(), &username, &password).await?;
                private_api::forget_session().await;
                done("Instant live is ready");
                settings.username = username;
                return Ok(());
            }
            Err(err) => fail(&login_message(&err)),
        }
        if !confirm("Try again?", true).await? {
            settings.username.clear();
            warn("Instant live skipped; live video uses RTSPS");
            return Ok(());
        }
    }
}

fn login_message(err: &anyhow::Error) -> String {
    match err.downcast_ref::<LoginError>() {
        Some(LoginError::BadCredentials) => "Wrong username or password".into(),
        Some(LoginError::MfaRequired) => {
            "This account uses MFA, which sauron can't answer. Use a local-only UniFi user instead".into()
        }
        Some(LoginError::RateLimited(_)) => "Too many login attempts; the console is refusing logins for now".into(),
        _ => format!("{err:#}"),
    }
}

fn done(message: &str) {
    println!("✓ {message}");
}

fn fail(message: &str) {
    println!("✗ {message}");
}

fn warn(message: &str) {
    println!("! {message}");
}

// ---- gum -------------------------------------------------------------------------------------

/// Runs gum on the terminal and returns what it printed; a non-zero exit is a cancellation.
/// (`Command::output` would pipe stderr too, hiding gum's interface, so spawn and wait instead.)
async fn gum(args: &[&str], stdout: Stdio) -> Result<String> {
    let output = Command::new("gum")
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(stdout)
        .stderr(Stdio::inherit())
        .spawn()
        .context("cannot run gum (is it installed?)")?
        .wait_with_output()
        .await?;
    if !output.status.success() {
        return Err(Cancelled.into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

async fn input(header: &str, value: &str, placeholder: &str, password: bool) -> Result<String> {
    let mut args = vec!["input", "--header", header, "--value", value, "--placeholder", placeholder];
    if password {
        args.push("--password");
    }
    gum(&args, Stdio::piped()).await
}

async fn confirm(prompt: &str, default: bool) -> Result<bool> {
    let status = Command::new("gum")
        .args(["confirm", prompt, if default { "--default=true" } else { "--default=false" }])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .context("cannot run gum (is it installed?)")?;
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(Cancelled.into()),
    }
}

/// Shows a gum spinner while `work` runs. gum only spins around a command, so it gets a
/// placeholder `sleep` and is stopped with SIGTERM (which restores the terminal) when done.
async fn spin<T>(title: &str, work: impl Future<Output = Result<T>>) -> Result<T> {
    let spinner = Command::new("gum")
        .args(["spin", "--spinner", "dot", "--title", title, "--", "sleep", "3600"])
        .stdin(Stdio::inherit())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn();
    let result = work.await;
    if let Ok(mut spinner) = spinner {
        if let Some(pid) = spinner.id().and_then(|pid| i32::try_from(pid).ok()) {
            // SAFETY: plain kill(2) on our own child, which has not been reaped yet.
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
        let _ = spinner.wait().await;
    }
    result
}
