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
    let Some(window) = clients.iter().find(|c| c.title == title) else {
        return Ok(false);
    };
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
    let url = api
        .livestream_url(&camera.id, private_api::channel(&config.live_quality))
        .await?;
    let (mut socket, _) = tokio_tungstenite::connect_async_tls_with_config(
        url.as_str(),
        None,
        true,
        Some(client.ws_connector()),
    )
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
            Err(_) => {
                break Err(anyhow!(
                    "livestream silent for {}s",
                    LIVESTREAM_SILENCE.as_secs()
                ));
            }
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
        return Err(ended
            .err()
            .unwrap_or_else(|| anyhow!("livestream closed before any video arrived")));
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
        .with_context(|| {
            format!(
                "cannot get {} stream for {}",
                config.live_quality,
                camera.display_name()
            )
        })?;
    let url = protect::fixup_stream_url(&url, client.hostname())?;
    // The player lives on in its own process group after we exit.
    let mut player = spawn_player(&config.player, title, &url, Stdio::null())?;
    let Some(pid) = player.id() else {
        return Ok(());
    };
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
    if Path::new(program)
        .file_name()
        .is_some_and(|name| name == "mpv")
    {
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
            let Some(payload) = self.pending.get(offset + 4..offset + 4 + length) else {
                break;
            };
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

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(kind: u8, payload: &[u8]) -> Vec<u8> {
        let length = (payload.len() as u32).to_be_bytes();
        [&[kind], &length[1..], payload].concat()
    }

    /// A media segment whose frames arrive in a different order than they must be written.
    fn segment(tag: &str) -> Vec<u8> {
        [
            frame(frame::BEGIN, b""),
            frame(frame::TIMESTAMP, b"\x00\x00\x00\x2a"),
            frame(frame::AUDIO, format!("audio{tag}").as_bytes()),
            frame(frame::VIDEO, format!("video{tag}").as_bytes()),
            frame(frame::MDAT, format!("mdat{tag}").as_bytes()),
            frame(frame::MOOF, format!("moof{tag}").as_bytes()),
            frame(frame::END, b""),
        ]
        .concat()
    }

    fn demux(messages: &[&[u8]]) -> Vec<u8> {
        let mut demux = Demux::default();
        let mut out = Vec::new();
        for message in messages {
            demux.push(message, &mut out);
        }
        out
    }

    #[test]
    fn writes_init_then_each_segment_as_moof_mdat_video_audio() {
        let stream = [frame(frame::INIT, b"INIT"), segment("1"), segment("2")].concat();
        assert_eq!(
            demux(&[&stream]),
            b"INITmoof1mdat1video1audio1moof2mdat2video2audio2"
        );
    }

    #[test]
    fn frames_may_span_websocket_messages() {
        let stream = [frame(frame::INIT, b"INIT"), segment("1"), segment("2")].concat();
        let whole = demux(&[&stream]);
        for size in [1, 3, 5, 17] {
            let messages: Vec<&[u8]> = stream.chunks(size).collect();
            assert_eq!(demux(&messages), whole, "chunks of {size}");
        }

        // Nothing is written until the segment's end frame is complete.
        let (head, tail) = stream.split_at(stream.len() - 5);
        let mut demuxer = Demux::default();
        let mut out = Vec::new();
        demuxer.push(head, &mut out);
        assert_eq!(out, b"INITmoof1mdat1video1audio1");
        demuxer.push(tail, &mut out);
        assert_eq!(out, whole);
    }

    #[test]
    fn init_is_written_once_and_media_waits_for_it() {
        let stream = [
            segment("0"),
            frame(frame::INIT, b"INIT"),
            segment("1"),
            frame(frame::INIT, b"AGAIN"),
            segment("2"),
        ]
        .concat();
        assert_eq!(
            demux(&[&stream]),
            b"INITmoof1mdat1video1audio1moof2mdat2video2audio2"
        );
    }

    #[test]
    fn begin_discards_a_partial_segment_and_unknown_frames_are_skipped() {
        let stream = [
            frame(frame::INIT, b"INIT"),
            frame(frame::MOOF, b"stale"),
            frame(248, b"avc1.640028"),
            segment("1"),
        ]
        .concat();
        assert_eq!(demux(&[&stream]), b"INITmoof1mdat1video1audio1");
    }

    #[test]
    fn lost_sync_drops_the_message_and_recovers_on_the_next() {
        // Not a frame type: its "length" would otherwise stall the stream waiting for 16 MiB.
        let garbage = [
            frame(frame::MOOF, b"half"),
            vec![0x01, 0xff, 0xff, 0xff, 0x00],
        ]
        .concat();
        let out = demux(&[&frame(frame::INIT, b"INIT"), &garbage, &segment("1")]);
        assert_eq!(out, b"INITmoof1mdat1video1audio1");
    }
}
