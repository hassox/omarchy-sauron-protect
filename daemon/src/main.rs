mod config;
mod daemon;
mod live;
mod notify;
mod proto;
mod protect;

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};

use crate::protect::{Camera, Protect};

const USAGE: &str = "\
sauron — UniFi Protect bridge for the Omarchy bar

usage:
  sauron watch                      run the daemon (JSON lines on stdin/stdout)
  sauron check                      verify config and list cameras
  sauron cameras                    list cameras
  sauron live <camera>              open the live view (camera id or name)
  sauron snapshot <camera> [out]    save a high-quality snapshot (default ./<name>.jpg)
  sauron config                     create the config file if missing and print its path
  sauron --version                  print the version
";

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    // Only fails if a provider is already installed, which cannot happen this early.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match args.as_slice() {
        ["watch"] => daemon::run().await,
        ["check"] => list_cameras(true).await,
        ["cameras"] => list_cameras(false).await,
        ["live", camera] => live(camera).await,
        ["snapshot", camera] => snapshot(camera, None).await,
        ["snapshot", camera, out] => snapshot(camera, Some(out)).await,
        ["config"] => create_config().await,
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

async fn sorted_cameras(client: &Protect) -> Result<Vec<Camera>> {
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
fn resolve<'a>(cameras: &'a [Camera], query: &str) -> Result<&'a Camera> {
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

async fn live(query: &str) -> Result<()> {
    let (config, client) = connect().await?;
    let cameras = sorted_cameras(&client).await?;
    let camera = resolve(&cameras, query)?;
    let url = client
        .stream_url(&camera.id, &config.live_quality)
        .await
        .with_context(|| format!("cannot get {} stream for {}", config.live_quality, camera.display_name()))?;
    let url = protect::fixup_stream_url(&url, client.hostname())?;
    // Not awaited: the player lives on in its own process group after we exit.
    live::spawn_player(&config.player, camera.display_name(), &url)?;
    Ok(())
}

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
        .snapshot(&camera.id, true)
        .await
        .with_context(|| format!("cannot fetch snapshot of {}", camera.display_name()))?;
    tokio::fs::write(&path, &jpeg).await.with_context(|| format!("cannot write {}", path.display()))?;
    println!("{}", path.display());
    Ok(())
}

async fn create_config() -> Result<()> {
    let path = config::config_path();
    if config::write_template(&path).await? {
        eprintln!("created config template; add your API key");
    }
    println!("{}", path.display());
    Ok(())
}
