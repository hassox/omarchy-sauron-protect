mod config;
mod daemon;
mod event_log;
mod history;
mod hypr;
mod keyring;
mod live;
mod notify;
mod private_api;
mod protect;
mod proto;
mod setup;
mod tls;

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};

use crate::protect::{Camera, ConnectError, Failure, Outcome, Protect};

const USAGE: &str = "\
sauron — UniFi Protect bridge for the Omarchy bar

usage:
  sauron setup                      set up (or change) the console, API key and instant live
  sauron watch                      run the daemon (JSON lines on stdin/stdout)
  sauron check                      verify config and list cameras
  sauron cameras                    list cameras
  sauron live <camera>              open the live view (camera id or name)
  sauron snapshot <camera> [out]    save a snapshot (default ./<name>.jpg)
  sauron log [--since <when>] [--until <when>] [--all]
                                    print Protect's event history as JSON lines, oldest first;
                                    <when> is 30m, 24h, 7d, YYYY-MM-DD, \"YYYY-MM-DD HH:MM\" or
                                    RFC 3339 (default: the last 24h). --all adds every other event
                                    type, including admin logins with IP addresses and presence
  sauron thumbnail <event-id> [out] save an event's thumbnail (default ./<event-id>.jpg)
  sauron --version                  print the version
";

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    // Only fails if a provider is already installed, which cannot happen this early.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match args.as_slice() {
        ["setup"] => return setup::run().await,
        ["watch"] => daemon::run().await,
        ["check"] => check().await,
        ["cameras"] => cameras().await,
        ["live", camera] => live::run(camera).await,
        ["snapshot", camera] => snapshot(camera, None).await,
        ["snapshot", camera, out] => snapshot(camera, Some(out)).await,
        ["log", rest @ ..] => match history::LogArgs::parse(rest) {
            Some(args) => history::log(args).await,
            None => {
                eprint!("{USAGE}");
                return ExitCode::from(2);
            }
        },
        ["thumbnail", id] => history::thumbnail(id, None).await,
        ["thumbnail", id, out] => history::thumbnail(id, Some(out)).await,
        ["--version" | "-V"] => {
            println!("sauron {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        [] | ["help" | "-h" | "--help"] => {
            print!("{USAGE}");
            Ok(())
        }
        _ => {
            eprint!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("sauron: {err:#}");
            ExitCode::FAILURE
        }
    }
}

/// Loads the config and connects to the first address of the console that answers.
pub async fn connect() -> Result<(config::Config, Protect, String)> {
    let config = config::load(&config::config_path()).await?;
    let (client, version) = protect::connect(&config).await?;
    Ok((config, client, version))
}

pub async fn sorted_cameras(client: &Protect) -> Result<Vec<Camera>> {
    let mut cameras = client.cameras().await.context("cannot list cameras")?;
    cameras.sort_by_cached_key(|c| c.display_name().to_lowercase());
    Ok(cameras)
}

async fn cameras() -> Result<()> {
    list_cameras(&connect().await?.1).await
}

async fn list_cameras(client: &Protect) -> Result<()> {
    for camera in sorted_cameras(client).await? {
        let state = if camera.online() { "online" } else { "offline" };
        println!(
            "{state}\t{}\t{}\t{}",
            camera.display_name(),
            camera.model(),
            camera.id
        );
    }
    Ok(())
}

/// `Protect <v> at <address> (<security>)` and the cameras, or one line per address tried.
async fn check() -> Result<()> {
    let config = config::load(&config::config_path()).await?;
    let (client, version) = match protect::connect(&config).await {
        Ok(connected) => connected,
        Err(err) => return Err(report(&err)),
    };
    let via = if client.via().is_some() {
        " via fallback"
    } else {
        ""
    };
    println!(
        "Protect {version} at {} ({}){via}",
        client.address(),
        client.security()
    );
    list_cameras(&client).await
}

/// Prints each failed address with its outcome; returns the overall error.
fn report(err: &ConnectError) -> anyhow::Error {
    for attempt in &err.attempts {
        let detail = match attempt.outcome {
            Outcome::NoAnswer | Outcome::Other(_) => format!(" ({})", root_cause(&attempt.error)),
            _ => String::new(),
        };
        eprintln!("✗ {}: {}{detail}", attempt.address, attempt.outcome);
    }
    match err.failure() {
        Failure::Offline => anyhow::anyhow!("cannot connect to the console"),
        Failure::Auth | Failure::Untrusted => anyhow::anyhow!("{err}"),
    }
}

/// The innermost cause, looking inside transport errors: "Connection refused (os error 111)".
fn root_cause(err: &anyhow::Error) -> String {
    let mut cause: &(dyn std::error::Error + 'static) = err.root_cause();
    if let Some(transport) = cause.downcast_ref::<protect::Transport>() {
        cause = transport.inner();
        while let Some(source) = cause.source() {
            cause = source;
        }
    }
    cause.to_string()
}

/// Finds a camera by id, case-insensitive name, or unique case-insensitive name prefix.
pub fn resolve<'a>(cameras: &'a [Camera], query: &str) -> Result<&'a Camera> {
    if let Some(camera) = cameras.iter().find(|c| c.id == query) {
        return Ok(camera);
    }
    let query = query.to_lowercase();
    if let Some(camera) = cameras
        .iter()
        .find(|c| c.display_name().to_lowercase() == query)
    {
        return Ok(camera);
    }
    let matches: Vec<&Camera> = cameras
        .iter()
        .filter(|c| c.display_name().to_lowercase().starts_with(&query))
        .collect();
    match matches.as_slice() {
        [camera] => Ok(camera),
        [] => bail!("no camera matches {query:?}"),
        many => {
            let names: Vec<&str> = many.iter().map(|c| c.display_name()).collect();
            bail!("{query:?} is ambiguous: {}", names.join(", "))
        }
    }
}

/// Saves the best snapshot the camera offers (full HD only where its feature flags allow it).
async fn snapshot(query: &str, out: Option<&str>) -> Result<()> {
    let (_, client, _) = connect().await?;
    let cameras = sorted_cameras(&client).await?;
    let camera = resolve(&cameras, query)?;
    let path = match out {
        Some(out) => PathBuf::from(out),
        None => {
            let name: String = camera
                .display_name()
                .chars()
                .map(|c| {
                    if c.is_alphanumeric() || c == '-' || c == '_' {
                        c
                    } else {
                        '_'
                    }
                })
                .collect();
            PathBuf::from(format!("./{name}.jpg"))
        }
    };
    let jpeg = client
        .snapshot(&camera.id, camera.supports_full_hd_snapshot())
        .await
        .with_context(|| format!("cannot fetch snapshot of {}", camera.display_name()))?;
    tokio::fs::write(&path, &jpeg)
        .await
        .with_context(|| format!("cannot write {}", path.display()))?;
    println!("{}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cameras(names: &[(&str, &str)]) -> Vec<Camera> {
        names
            .iter()
            .map(|&(id, name)| Camera {
                id: id.into(),
                name: Some(name.into()),
                ..Camera::default()
            })
            .collect()
    }

    #[test]
    fn resolve_by_id_exact_name_then_unique_prefix() {
        let cameras = cameras(&[
            ("cam-a", "Garage"),
            ("cam-b", "Garage Side"),
            ("cam-c", "Front Door"),
        ]);
        let id = |query: &str| {
            resolve(&cameras, query)
                .map(|c| c.id.as_str())
                .map_err(|e| e.to_string())
        };
        assert_eq!(id("cam-c"), Ok("cam-c"));
        // An exact name wins although it is also a prefix of another camera's name.
        assert_eq!(id("gARAGE"), Ok("cam-a"));
        assert_eq!(id("fro"), Ok("cam-c"));
        assert_eq!(id("garage s"), Ok("cam-b"));
    }

    #[test]
    fn resolve_reports_ambiguous_and_unknown_queries() {
        let cameras = cameras(&[
            ("cam-a", "Garage"),
            ("cam-b", "Garage Side"),
            ("cam-c", "Front Door"),
        ]);
        let error = |query: &str| resolve(&cameras, query).err().unwrap().to_string();
        assert_eq!(error("Gar"), "\"gar\" is ambiguous: Garage, Garage Side");
        assert_eq!(error("back"), "no camera matches \"back\"");
    }
}
