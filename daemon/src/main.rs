mod config;
mod daemon;
mod hypr;
mod keyring;
mod live;
mod notify;
mod private_api;
mod proto;
mod protect;
mod setup;

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};

use crate::protect::{Camera, Protect};

const USAGE: &str = "\
sauron — UniFi Protect bridge for the Omarchy bar

usage:
  sauron setup                      set up (or change) the console, API key and instant live
  sauron watch                      run the daemon (JSON lines on stdin/stdout)
  sauron check                      verify config and list cameras
  sauron cameras                    list cameras
  sauron live <camera>              open the live view (camera id or name)
  sauron snapshot <camera> [out]    save a snapshot (default ./<name>.jpg)
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
        ["check"] => list_cameras(true).await,
        ["cameras"] => list_cameras(false).await,
        ["live", camera] => live::run(camera).await,
        ["snapshot", camera] => snapshot(camera, None).await,
        ["snapshot", camera, out] => snapshot(camera, Some(out)).await,
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

async fn connect() -> Result<(config::Config, Protect)> {
    let config = config::load(&config::config_path()).await?;
    let client = Protect::new(&config)?;
    Ok((config, client))
}

pub async fn sorted_cameras(client: &Protect) -> Result<Vec<Camera>> {
    let mut cameras = client.cameras().await.context("cannot list cameras")?;
    cameras.sort_by_cached_key(|c| c.display_name().to_lowercase());
    Ok(cameras)
}

async fn list_cameras(header: bool) -> Result<()> {
    let (config, client) = connect().await?;
    if header {
        let version = client.version().await.with_context(|| format!("Protect at {}", config.host))?;
        println!("Protect {version} at {}", config.host);
    }
    for camera in sorted_cameras(&client).await? {
        let state = if camera.online() { "online" } else { "offline" };
        println!("{state}\t{}\t{}\t{}", camera.display_name(), camera.model(), camera.id);
    }
    Ok(())
}

/// Finds a camera by id, case-insensitive name, or unique case-insensitive name prefix.
pub fn resolve<'a>(cameras: &'a [Camera], query: &str) -> Result<&'a Camera> {
    if let Some(camera) = cameras.iter().find(|c| c.id == query) {
        return Ok(camera);
    }
    let query = query.to_lowercase();
    if let Some(camera) = cameras.iter().find(|c| c.display_name().to_lowercase() == query) {
        return Ok(camera);
    }
    let matches: Vec<&Camera> =
        cameras.iter().filter(|c| c.display_name().to_lowercase().starts_with(&query)).collect();
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
    let (_, client) = connect().await?;
    let cameras = sorted_cameras(&client).await?;
    let camera = resolve(&cameras, query)?;
    let path = match out {
        Some(out) => PathBuf::from(out),
        None => {
            let name: String = camera
                .display_name()
                .chars()
                .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
                .collect();
            PathBuf::from(format!("./{name}.jpg"))
        }
    };
    let jpeg = client
        .snapshot(&camera.id, camera.supports_full_hd_snapshot())
        .await
        .with_context(|| format!("cannot fetch snapshot of {}", camera.display_name()))?;
    tokio::fs::write(&path, &jpeg).await.with_context(|| format!("cannot write {}", path.display()))?;
    println!("{}", path.display());
    Ok(())
}
