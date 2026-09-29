//! `sauron live <camera>`: the viewer process. Focuses an open viewer for the camera, else plays
//! Protect's private livestream (instant start) or, failing that, the RTSPS stream.

use std::io::ErrorKind;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use futures_util::StreamExt;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};
use tokio_tungstenite::tungstenite::Message;

use crate::config::Config;
use crate::private_api::{self, PrivateApi};
use crate::protect::{self, Camera, Protect};
use crate::{hypr, keyring};

/// Protect sends a fragment every 100 ms; this much silence means the stream is dead.
const LIVESTREAM_SILENCE: Duration = Duration::from_secs(10);

pub async fn run(query: &str) -> Result<()> {
    let (config, client, _) = crate::connect().await?;
    let cameras = crate::sorted_cameras(&client).await?;
    let camera = crate::resolve(&cameras, query)?;
    let title = format!("Sauron · {}", camera.display_name());

    match focus_open_viewer(&title).await {
        Ok(true) => return Ok(()),
        Ok(false) => {}
        Err(err) => eprintln!("sauron: {err:#}"),
    }
    if !config.username.is_empty() {
        match instant(&config, &client, camera, &title).await {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(err) => eprintln!("sauron: instant live failed, using RTSPS: {err:#}"),
        }
    }
    rtsps(&config, &client, camera, &title).await
}

/// Focuses the window titled `title` if one is open.
async fn focus_open_viewer(title: &str) -> Result<bool> {
    let clients = hypr::clients().await?;
    let Some(window) = clients.iter().find(|c| c.title == title) else { return Ok(false) };
    hypr::focus(&format!("address:{}", window.address)).await?;
    Ok(true)
}

/// Plays the private livestream. `Ok(false)`: no password in the keyring, so instant live is
/// not set up. `Err`: failed before any video reached the player, so RTSPS should take over.
async fn instant(config: &Config, client: &Protect, camera: &Camera, title: &str) -> Result<bool> {
    let Some(password) = keyring::lookup(client.console_id(), &config.username).await else {
        return Ok(false);
    };
    let mut api = PrivateApi::new(client, &config.username, password);
    let url = api.livestream_url(&camera.id, private_api::channel(&config.live_quality)).await?;
    let (mut socket, _) =
        tokio_tungstenite::connect_async_tls_with_config(url.as_str(), None, true, Some(client.ws_connector()))
            .await
            .map_err(|err| anyhow!("livestream websocket: {err}"))?;

    let mut player = spawn_player(&config.player, title, "-", Stdio::piped())?;
    let mut stdin = player.stdin.take().context("player has no stdin")?;
    let focus = player.id().map(|pid| tokio::spawn(focus_when_mapped(pid)));

    let mut demux = Demux::default();
    let mut out = Vec::new();
    let mut fed = false;
    let ended: Result<()> = loop {
        let frame = tokio::select! {
            _ = player.wait() => return Ok(true),
            frame = tokio::time::timeout(LIVESTREAM_SILENCE, socket.next()) => frame,
        };
        match frame {
            Err(_) => break Err(anyhow!("livestream silent for {}s", LIVESTREAM_SILENCE.as_secs())),
            Ok(None | Some(Ok(Message::Close(_)))) => break Ok(()),
            Ok(Some(Err(err))) => break Err(anyhow!("livestream websocket: {err}")),
            Ok(Some(Ok(Message::Binary(data)))) => {
                out.clear();
                demux.push(&data, &mut out);
                if out.is_empty() {
                    continue;
                }
                match stdin.write_all(&out).await {
                    Ok(()) => fed = true,
                    // The player quit: the viewer closed its window.
                    Err(err) if err.kind() == ErrorKind::BrokenPipe => return Ok(true),
                    Err(err) => break Err(anyhow!("writing to the player: {err}")),
                }
            }
            Ok(Some(Ok(_))) => {}
        }
    };

    if !fed {
        if let Some(focus) = focus {
            focus.abort();
        }
        let _ = player.kill().await;
        return Err(ended.err().unwrap_or_else(|| anyhow!("livestream closed before any video arrived")));
    }
    if let Err(err) = ended {
        eprintln!("sauron: {err:#}");
    }
    // End of stream: let the player show the last frames and exit on its own.
    drop(stdin);
    let _ = player.wait().await;
    Ok(true)
}

async fn rtsps(config: &Config, client: &Protect, camera: &Camera, title: &str) -> Result<()> {
    let url = client
        .stream_url(&camera.id, &config.live_quality)
        .await
        .with_context(|| format!("cannot get {} stream for {}", config.live_quality, camera.display_name()))?;
    let url = protect::fixup_stream_url(&url, client.hostname())?;
    // The player lives on in its own process group after we exit.
    let mut player = spawn_player(&config.player, title, &url, Stdio::null())?;
    let Some(pid) = player.id() else { return Ok(()) };
    tokio::select! {
        () = focus_when_mapped(pid) => Ok(()),
        status = player.wait() => match status {
            Ok(status) if !status.success() => Err(anyhow!("player exited ({status})")),
            _ => Ok(()),
        },
    }
}

async fn focus_when_mapped(pid: u32) {
    if let Err(err) = hypr::focus_when_mapped(pid).await {
        eprintln!("sauron: {err:#}");
    }
}

/// Spawns the configured player for `target` (a URL, or `-` for stdin) in its own process group.
fn spawn_player(player: &[String], title: &str, target: &str, stdin: Stdio) -> Result<Child> {
    let (program, args) = player.split_first().context("`player` is empty")?;
    let mut command = Command::new(program);
    command.args(args);
    if Path::new(program).file_name().is_some_and(|name| name == "mpv") {
        command.arg(format!("--title={title}"));
    }
    command
        .arg(target)
        .stdin(stdin)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .with_context(|| format!("cannot start player `{program}`"))
}

/// Protect livestream frame types: each frame is `[type u8][length u24 BE][payload]`, and frames
/// may span websocket messages (hjdhjd/unifi-protect src/transport/livestream-session.ts).
mod frame {
    pub const TIMESTAMP: u8 = 247;
    pub const INIT: u8 = 250;
    pub const BEGIN: u8 = 249;
    pub const MOOF: u8 = 251;
    pub const VIDEO: u8 = 252;
    pub const AUDIO: u8 = 253;
    pub const MDAT: u8 = 254;
    pub const END: u8 = 255;
}

/// Reassembles the livestream into a plain fMP4 byte stream: the init segment (ftyp+moov) once,
/// then each media segment as moof, mdat, video, audio (the order the controller uses).
#[derive(Default)]
struct Demux {
    pending: Vec<u8>,
    parts: [Vec<u8>; 4],
    init_sent: bool,
}

impl Demux {
    fn push(&mut self, message: &[u8], out: &mut Vec<u8>) {
        self.pending.extend_from_slice(message);
        let mut offset = 0;
        while let Some(header) = self.pending.get(offset..offset + 4) {
            let kind = header[0];
            let length = u32::from_be_bytes([0, header[1], header[2], header[3]]) as usize;
            if kind < frame::TIMESTAMP {
                // Lost frame sync: drop what we have and resynchronise on a later message.
                self.pending.clear();
                self.parts.iter_mut().for_each(Vec::clear);
                return;
            }
            let Some(payload) = self.pending.get(offset + 4..offset + 4 + length) else { break };
            match kind {
                frame::INIT if !self.init_sent => {
                    out.extend_from_slice(payload);
                    self.init_sent = true;
                }
                frame::BEGIN => self.parts.iter_mut().for_each(Vec::clear),
                frame::MOOF => self.parts[0].extend_from_slice(payload),
                frame::MDAT => self.parts[1].extend_from_slice(payload),
                frame::VIDEO => self.parts[2].extend_from_slice(payload),
                frame::AUDIO => self.parts[3].extend_from_slice(payload),
                frame::END => {
                    for part in &mut self.parts {
                        if self.init_sent {
                            out.extend_from_slice(part);
                        }
                        part.clear();
                    }
                }
                // Codec string, timestamps, repeated init.
                _ => {}
            }
            offset += 4 + length;
        }
        self.pending.drain(..offset);
    }
}
