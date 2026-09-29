//! `sauron setup`: a short interactive wizard, drawn with `gum` like Omarchy's own.

use std::fmt;
use std::io::IsTerminal;
use std::process::{ExitCode, Stdio};

use anyhow::{Context, Result};
use futures_util::future::join_all;
use reqwest::StatusCode;
use rustls::pki_types::CertificateDer;
use tokio::process::Command;

use crate::config::{self, Config, LoadError, Settings};
use crate::keyring;
use crate::private_api::{self, LoginError, PrivateApi};
use crate::protect::{self, Camera, Outcome, Protect};
use crate::tls::{self, Fingerprint, Rejection};

const KEY_HINT: &str = "Protect › Settings › Control Plane › Integrations › Create API key";
const ADDRESSES_HINT: &str =
    "Other addresses for this console, e.g. its Tailscale name or IP (optional, comma-separated)";
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
    let (host, pin) = ask_host(&settings).await?;
    settings.host = host;
    settings.cert_sha256 = pin.map(|pin| pin.to_string()).unwrap_or_default();
    let (client, cameras) = ask_api_key(&mut settings).await?;
    ask_other_addresses(&mut settings, pin).await?;
    ask_instant_live(&mut settings, &client, &cameras).await?;

    config::save(&path, &settings).await?;
    println!("Saved {}. The eye opens in a few seconds.", path.display());
    Ok(())
}

/// A keyless config for probing `host` (setup's checks send no API key).
fn probe_config(host: &str, pin: Option<Fingerprint>) -> Config {
    Config {
        host: host.to_owned(),
        fallback_hosts: Vec::new(),
        cert_sha256: pin,
        api_key: String::new(),
        username: String::new(),
        live_quality: String::new(),
        player: Vec::new(),
        event_log: None,
    }
}

/// Step 1: the console address, probed without a key (a Protect console answers 401), and its
/// certificate: trusted through webpki, or pinned once the user confirms it.
async fn ask_host(settings: &Settings) -> Result<(String, Option<Fingerprint>)> {
    let mut default = config::address(&settings.host);
    if default.is_empty() {
        default = config::default_gateway().await.map(|ip| ip.to_string()).unwrap_or_default();
    }
    let old_pin = Fingerprint::parse(&settings.cert_sha256);
    loop {
        let host = config::address(&input("Console address", &default, "IP or hostname", false).await?);
        if host.is_empty() {
            continue;
        }
        default.clone_from(&host);
        let (status, leaf, plain) = match spin("Looking for the console…", find_console(&host)).await {
            Ok(found) => found,
            Err(err) => {
                fail(&format!("Cannot reach {host}: {err:#}"));
                continue;
            }
        };
        if status == StatusCode::UNAUTHORIZED {
            done("UniFi console found");
        } else {
            warn(&format!("{host} answered HTTP {status}; it doesn't look like a UniFi console"));
            if !confirm("Continue anyway?", false).await? {
                continue;
            }
        }
        let Some(leaf) = leaf else {
            if plain {
                warn("Plain http has no TLS: for testing only");
            } else {
                done("Certificate trusted");
            }
            return Ok((host, None));
        };
        let pin = Fingerprint::of(&leaf);
        if old_pin.is_some_and(|old| old != pin) {
            warn("The console's certificate changed. Only trust it on your home network.");
        }
        let (common_name, self_signed) = tls::describe(&leaf).unwrap_or((None, false));
        let kind = if self_signed { "self-signed" } else { "not publicly trusted" };
        let name = common_name.map(|cn| format!(", CN={cn}")).unwrap_or_default();
        println!("Console certificate: SHA256 {} ({kind}{name})", pin.short());
        if confirm("Trust this console?", true).await? {
            return Ok((host, Some(pin)));
        }
    }
}

/// Probes `host` without a key: status, the leaf certificate when webpki doesn't trust it (the
/// probe then ran pinned to that leaf; the rejected handshake sent nothing), and whether the
/// address is plain http.
async fn find_console(host: &str) -> Result<(StatusCode, Option<CertificateDer<'static>>, bool)> {
    let trusted = Protect::new(&probe_config(host, None), host)?;
    let plain = trusted.security() == protect::Security::Plain;
    let err = match trusted.probe().await {
        Ok(status) => return Ok((status, None, plain)),
        Err(err) => err,
    };
    let Some(Rejection::Untrusted { leaf, .. }) = protect::rejection(&err) else { return Err(err) };
    let leaf = leaf.clone();
    let pinned = Protect::new(&probe_config(host, Some(Fingerprint::of(&leaf))), host)?;
    Ok((pinned.probe().await?, Some(leaf), plain))
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
        // Other addresses are checked in the next step.
        let keyed = Settings { fallback_hosts: Vec::new(), ..candidate.clone() };
        let config = match keyed.validate().await {
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
        let client = Protect::new(&config, &config.host)?;
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

/// Step 3 (optional): other addresses of the same console, each checked against the pin.
async fn ask_other_addresses(settings: &mut Settings, pin: Option<Fingerprint>) -> Result<()> {
    println!("{ADDRESSES_HINT}");
    let probe = probe_config(&settings.host, pin);
    let mut value = settings.fallback_hosts.join(", ");
    loop {
        value = input("Other addresses", &value, "none", false).await?;
        let mut addresses: Vec<String> = Vec::new();
        for address in value.split(',').map(config::address) {
            if !address.is_empty() && address != settings.host && !addresses.contains(&address) {
                addresses.push(address);
            }
        }
        let checked = if addresses.is_empty() {
            Vec::new()
        } else {
            let checks = join_all(addresses.iter().map(|address| check_address(&probe, address)));
            spin("Checking the addresses…", async { anyhow::Ok(checks.await) }).await?
        };
        let mut rejected = false;
        for (address, reach) in addresses.iter().zip(checked) {
            match reach {
                Reach::Console => done(&format!("{address} reaches your console")),
                Reach::Silent => warn(&format!("{address} doesn't answer right now; kept for later")),
                Reach::Different => {
                    fail(&format!("{address} is a different machine"));
                    rejected = true;
                }
                Reach::Untrusted => {
                    fail(&format!("{address} has an untrusted certificate"));
                    rejected = true;
                }
                Reach::Invalid(err) => {
                    fail(&format!("{address}: {err:#}"));
                    rejected = true;
                }
            }
        }
        if !rejected {
            settings.fallback_hosts = addresses;
            return Ok(());
        }
    }
}

/// What answers at another address of the console.
enum Reach {
    /// Our console: the pinned (or trusted) certificate, and an HTTP answer.
    Console,
    /// Nothing right now; it may answer later (e.g. Tailscale is down).
    Silent,
    /// A different certificate: another machine.
    Different,
    /// No pin, and webpki doesn't trust this address.
    Untrusted,
    Invalid(anyhow::Error),
}

/// Probes `address` without a key, under the console's pin.
async fn check_address(probe: &Config, address: &str) -> Reach {
    let client = match Protect::new(probe, address) {
        Ok(client) => client,
        Err(err) => return Reach::Invalid(err),
    };
    match client.probe().await {
        Ok(_) => Reach::Console,
        Err(err) => match Outcome::of(&err) {
            Outcome::NotYourConsole => Reach::Different,
            Outcome::Untrusted => Reach::Untrusted,
            _ => Reach::Silent,
        },
    }
}

/// Step 4 (optional): a local UniFi OS user for the private livestream; password to the keyring.
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
        if username.is_empty() {
            fail("A username is needed");
            continue;
        }
        let saved = keyring::lookup(client.console_id(), &username).await;
        let placeholder = if saved.is_some() { "Enter keeps the saved password" } else { "" };
        let typed = input("Password", "", placeholder, true).await?;
        let Some(password) = Some(typed).filter(|p| !p.is_empty()).or(saved) else {
            fail("A password is needed");
            continue;
        };
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
                keyring::store(client.console_id(), &username, &password).await?;
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
