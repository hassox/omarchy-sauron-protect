"""`sauron live <camera>`: the viewer process. Focuses an open viewer for the camera, else plays
Protect's private livestream (instant start) or, failing that, the RTSPS stream."""

from __future__ import annotations

import asyncio
import os
import sys

from . import hypr, keyring, net, private_api, protect
from .config import Config

# Protect sends a fragment every 100 ms; this much silence means the stream is dead.
LIVESTREAM_SILENCE = 10.0

# Livestream frames are `[type u8][length u24 BE][payload]` and may span websocket messages
# (hjdhjd/unifi-protect src/transport/livestream-session.ts).
TIMESTAMP, BEGIN, INIT, MOOF, VIDEO, AUDIO, MDAT, END = 247, 249, 250, 251, 252, 253, 254, 255
_PART = {MOOF: 0, MDAT: 1, VIDEO: 2, AUDIO: 3}


class Demux:
    """Reassembles the livestream into plain fMP4: the init segment (ftyp+moov) once, then each
    media segment as moof, mdat, video, audio (the order the controller uses)."""

    def __init__(self):
        self.pending = bytearray()
        self.parts = [bytearray() for _ in range(4)]
        self.init_sent = False

    def push(self, message: bytes) -> bytes:
        self.pending += message
        out = bytearray()
        offset = 0
        while offset + 4 <= len(self.pending):
            kind = self.pending[offset]
            length = int.from_bytes(self.pending[offset + 1: offset + 4], "big")
            if kind < TIMESTAMP:
                # Lost frame sync: drop what we have and resynchronise on a later message.
                self.pending.clear()
                for part in self.parts:
                    part.clear()
                return bytes(out)
            if offset + 4 + length > len(self.pending):
                break
            payload = self.pending[offset + 4: offset + 4 + length]
            if kind == INIT and not self.init_sent:
                out += payload
                self.init_sent = True
            elif kind == BEGIN:
                for part in self.parts:
                    part.clear()
            elif kind in _PART:
                self.parts[_PART[kind]] += payload
            elif kind == END:
                for part in self.parts:
                    if self.init_sent:
                        out += part
                    part.clear()
            offset += 4 + length
        del self.pending[:offset]
        return bytes(out)


def player_command(player: list[str], title: str, url: str | None) -> tuple[list[str], bool]:
    """The player's argv, and whether the URL goes in on stdin. With `url` None the player reads
    fMP4 from stdin. A URL carries the camera's access token, so mpv gets it as a one-line
    playlist on stdin rather than on its command line, which any local user can read."""
    argv = list(player)
    is_mpv = os.path.basename(argv[0]) == "mpv"
    if is_mpv:
        argv.append(f"--title={title}")
    if url is None:
        return argv + ["-"], False
    if is_mpv:
        return argv + ["--playlist=-"], True
    return argv + [url], False


async def spawn_player(player: list[str], title: str, url: str | None):
    argv, url_on_stdin = player_command(player, title, url)
    piped = url is None or url_on_stdin
    try:
        process = await asyncio.create_subprocess_exec(
            *argv,
            stdin=asyncio.subprocess.PIPE if piped else asyncio.subprocess.DEVNULL,
            stdout=asyncio.subprocess.DEVNULL,
            stderr=asyncio.subprocess.DEVNULL,
            process_group=0,  # outlives us when we exit
        )
    except OSError as err:
        raise RuntimeError(f"cannot start player `{argv[0]}`: {err.strerror or err}") from None
    if url_on_stdin:
        process.stdin.write(f"{url}\n".encode())
        await process.stdin.drain()
        process.stdin.close()
    return process


async def _focus_open_viewer(title: str) -> bool:
    for window in await hypr.clients():
        if window.get("title") == title:
            await hypr.focus(f"address:{window.get('address')}")
            return True
    return False


async def _focus_when_mapped(pid: int) -> None:
    try:
        await hypr.focus_when_mapped(pid)
    except (RuntimeError, OSError) as err:
        print(f"sauron: {err}", file=sys.stderr)


async def instant(config: Config, client: protect.Protect, camera: protect.Camera,
                  title: str) -> bool:
    """Plays the private livestream. False: instant live isn't set up (no password). Raises when
    it failed before any video reached the player, so RTSPS can take over."""
    password = await keyring.lookup(client.console_id, config.username)
    if not password:
        return False
    api = private_api.PrivateApi(client, config.username, password)
    url = await api.livestream_url(camera.id, private_api.channel(config.live_quality))
    socket = await net.WebSocket.connect(url, config.pin, max_size=None)
    player = await spawn_player(list(config.player), title, None)
    focus = asyncio.create_task(_focus_when_mapped(player.pid))
    demux = Demux()
    fed = False
    ended: BaseException | None = None
    player_gone = asyncio.create_task(player.wait())
    try:
        while True:
            receive = asyncio.create_task(socket.recv())
            done, _ = await asyncio.wait({receive, player_gone}, timeout=LIVESTREAM_SILENCE,
                                         return_when=asyncio.FIRST_COMPLETED)
            if player_gone in done:
                receive.cancel()
                return True  # the viewer closed its window
            if not done:
                receive.cancel()
                ended = RuntimeError(f"livestream silent for {LIVESTREAM_SILENCE:g}s")
                break
            try:
                message = receive.result()
            except net.WsClosed:
                break
            except net.TransportError as err:
                ended = err
                break
            if isinstance(message, str):
                continue
            out = demux.push(message)
            if not out:
                continue
            try:
                player.stdin.write(out)
                await player.stdin.drain()
                fed = True
            except (BrokenPipeError, ConnectionResetError):
                return True
    finally:
        await socket.close()
    if not fed:
        focus.cancel()
        player.kill()
        await player.wait()
        raise ended or RuntimeError("livestream closed before any video arrived")
    if ended:
        print(f"sauron: {ended}", file=sys.stderr)
    # End of stream: let the player show the last frames and exit on its own.
    player.stdin.close()
    await player_gone
    return True


async def rtsps(config: Config, client: protect.Protect, camera: protect.Camera, title: str) -> None:
    try:
        url = await client.stream_url(camera.id, config.live_quality)
    except Exception as err:
        raise RuntimeError(f"cannot get {config.live_quality} stream for {camera.display_name}: {err}") from err
    url = protect.fixup_stream_url(url, client.hostname)
    player = await spawn_player(list(config.player), title, url)
    focus = asyncio.create_task(_focus_when_mapped(player.pid))
    exited = asyncio.create_task(player.wait())
    done, _ = await asyncio.wait({focus, exited}, return_when=asyncio.FIRST_COMPLETED)
    if exited in done and exited.result() != 0:
        raise RuntimeError(f"player exited ({exited.result()})")


async def run(config: Config, client: protect.Protect, query: str) -> None:
    cameras = protect.sorted_cameras(await client.cameras())
    camera = protect.resolve(cameras, query)
    title = f"Sauron · {camera.display_name}"
    try:
        if await _focus_open_viewer(title):
            return
    except (RuntimeError, OSError) as err:
        print(f"sauron: {err}", file=sys.stderr)
    if config.username:
        try:
            if await instant(config, client, camera, title):
                return
        except Exception as err:  # noqa: BLE001  any failure falls back to RTSPS
            print(f"sauron: instant live failed, using RTSPS: {err}", file=sys.stderr)
    await rtsps(config, client, camera, title)
