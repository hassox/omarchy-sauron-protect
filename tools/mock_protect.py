#!/usr/bin/env python3
"""A fake UniFi Protect console for development and the end-to-end tests.

It serves what sauron uses: the Integration API (meta/info, cameras, snapshots, RTSPS streams, the
events and devices websockets), the UniFi OS login, and Protect's private API (livestream, event
history, event thumbnails). The API key is `mock`; the UniFi OS user is `sauron` with password
`mock`, and the user `mfa` (same password) behaves like an account with MFA.

    python3 tools/mock_protect.py [--port 7447] [--tls] [--interval 20] [--scenes DIR]

Point sauron at `http://127.0.0.1:7447` (or `127.0.0.1:7447` with --tls, which prints the
certificate to trust). `curl -X POST 'http://127.0.0.1:7447/mock/trigger?camera=Driveway&kind=person'`
fires a detection. The livestream sends placeholder frames, not real video.
"""

from __future__ import annotations

import argparse
import asyncio
import base64
import json
import os
import secrets
import ssl
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass, field
from pathlib import Path
from urllib.parse import parse_qs, urlsplit

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "sauron" / "_vendor"))

import h11  # noqa: E402
from websockets.http11 import Request as WsRequest  # noqa: E402
from websockets.server import ServerProtocol  # noqa: E402

API_KEY = "mock"
API = "/proxy/protect/integration"
USER, MFA_USER, PASSWORD = "sauron", "mfa", "mock"
LOGIN_FAILURES_ALLOWED = 5
INTERNAL_HOST = "unifi.internal"

# A 1x1 grey JPEG; --scenes swaps in real pictures.
PIXEL = base64.b64decode(
    "/9j/4AAQSkZJRgABAQEASABIAAD/2wBDAP//////////////////////////////////////////////////////"
    "////////////////////////////////wgALCAABAAEBAREA/8QAFBABAAAAAAAAAAAAAAAAAAAAAP/aAAgBAQAB"
    "PxA="
)

CAMERAS = [
    ("cam-front", "Front Door", "UVC G4 Doorbell Pro", ["person", "vehicle", "package", "face"]),
    ("cam-drive", "Driveway", "UVC G5 Bullet", ["person", "vehicle", "licensePlate"]),
    ("cam-back", "Backyard", "UVC G5 Flex", ["person", "animal"]),
    ("cam-garage", "Garage", "UVC G4 Instant", ["person"]),
    ("cam-side", "Side Gate", "UVC G5 Turret Ultra", ["person", "animal"]),
]


@dataclass
class Seen:
    """One request as the console received it, for tests to inspect."""

    method: str
    path: str
    headers: dict[str, str]


@dataclass
class MockConsole:
    port: int = 0
    host: str = "127.0.0.1"
    tls: ssl.SSLContext | None = None
    scenes: Path | None = None
    # Answer Integration API calls with a redirect to this URL instead (tests).
    redirect_to: str | None = None
    livestream_fails: bool = False
    api_key: str = API_KEY
    password: str = PASSWORD
    seen: list[Seen] = field(default_factory=list)
    sessions: set[str] = field(default_factory=set)
    login_failures: list[float] = field(default_factory=list)
    offline: set[str] = field(default_factory=set)
    history: list[dict] = field(default_factory=list)
    _event_sockets: list = field(default_factory=list)
    _device_sockets: list = field(default_factory=list)
    _server: asyncio.Server | None = None

    # ------------------------------------------------------------------ lifecycle

    async def start(self) -> MockConsole:
        self._server = await asyncio.start_server(self._serve, self.host, self.port, ssl=self.tls)
        self.port = self._server.sockets[0].getsockname()[1]
        if not self.history:
            self.history = _made_up_history()
        return self

    async def stop(self) -> None:
        if self._server:
            self._server.close()
            for writer in list(self._event_sockets) + list(self._device_sockets):
                writer[1].close()
            await self._server.wait_closed()

    @property
    def address(self) -> str:
        scheme = "https" if self.tls else "http"
        return f"{scheme}://{self.host}:{self.port}" if not self.tls else f"{self.host}:{self.port}"

    # ------------------------------------------------------------------ driving it

    async def trigger(self, camera: str, kind: str = "person", duration: float = 0.2) -> str:
        """Fires a detection on `camera` (id or name): an add, then an update with its end."""
        cam = next(c for c in CAMERAS if camera in (c[0], c[1]))
        event_id = "ev-" + secrets.token_hex(6)
        start = int(time.time() * 1000)
        if kind == "ring":
            event_type, types = "ring", []
        elif kind == "motion":
            event_type, types = "motion", []
        else:
            event_type, types = "smartDetectZone", [kind]
        item = {"id": event_id, "type": event_type, "start": start, "device": cam[0],
                "smartDetectTypes": types}
        await self._broadcast(self._event_sockets, {"type": "add", "item": item})
        await asyncio.sleep(duration)
        end = int(time.time() * 1000)
        await self._broadcast(self._event_sockets,
                              {"type": "update", "item": {"id": event_id, "end": end}})
        self.history.append({**item, "end": end, "camera": cam[0]})
        return event_id

    async def rename(self, camera_id: str, name: str) -> None:
        await self._broadcast(self._device_sockets, {
            "type": "update", "item": {"id": camera_id, "modelKey": "camera", "name": name}})

    async def _broadcast(self, sockets: list, frame: dict) -> None:
        text = json.dumps(frame)
        for protocol, writer in list(sockets):
            try:
                protocol.send_text(text.encode())
                await _flush(protocol, writer)
            except (OSError, ConnectionError):
                sockets.remove((protocol, writer))

    # ------------------------------------------------------------------ connections

    async def _serve(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        try:
            head = await reader.readuntil(b"\r\n\r\n")
        except (asyncio.IncompleteReadError, asyncio.LimitOverrunError, ConnectionError, ssl.SSLError):
            writer.close()
            return
        if b"upgrade: websocket" in head.lower():
            await self._websocket(head, reader, writer)
        else:
            await self._http(head, reader, writer)

    async def _http(self, head: bytes, reader, writer) -> None:
        conn = h11.Connection(h11.SERVER)
        conn.receive_data(head)
        try:
            while True:
                request, body = None, b""
                while True:
                    event = conn.next_event()
                    if event is h11.NEED_DATA:
                        data = await reader.read(65536)
                        conn.receive_data(data)
                        if not data and conn.their_state is h11.IDLE:
                            return
                        continue
                    if isinstance(event, h11.ConnectionClosed):
                        return
                    if isinstance(event, h11.Request):
                        request = event
                    elif isinstance(event, h11.Data):
                        body += event.data
                    elif isinstance(event, h11.EndOfMessage):
                        break
                assert request is not None
                headers = {k.decode().lower(): v.decode() for k, v in request.headers}
                target = request.target.decode()
                self.seen.append(Seen(request.method.decode(), target, headers))
                status, extra, payload = self._route(request.method.decode(), target, headers, body)
                response_headers = [("Content-Length", str(len(payload)))] + extra
                writer.write(conn.send(h11.Response(status_code=status, headers=response_headers)))
                writer.write(conn.send(h11.Data(data=payload)) if payload else b"")
                writer.write(conn.send(h11.EndOfMessage()))
                await writer.drain()
                if conn.our_state is h11.MUST_CLOSE:
                    return
                conn.start_next_cycle()
        except (h11.ProtocolError, ConnectionError, ssl.SSLError, OSError):
            return
        finally:
            writer.close()

    def _route(self, method: str, target: str, headers: dict, body: bytes):
        parts = urlsplit(target)
        path, query = parts.path, parse_qs(parts.query, keep_blank_values=True)
        json_type = [("Content-Type", "application/json")]

        def reply(status, value, extra=()):
            return status, json_type + list(extra), json.dumps(value).encode()

        if path.startswith("/mock/"):
            return self._control(method, path, query)
        if path == "/" and method == "GET":
            return 200, [("X-CSRF-Token", "csrf-root")], b"<html></html>"
        if path == "/api/auth/login" and method == "POST":
            return self._login(body)
        if path.startswith("/proxy/protect/api/"):
            return self._private(method, path, query, headers)
        if not path.startswith(API):
            return reply(404, {"error": "not found"})
        if self.redirect_to:
            return 302, [("Location", self.redirect_to + path)], b""
        if headers.get("x-api-key") != self.api_key:
            return reply(401, {"error": "Failed to authenticate request using 'apiKey'",
                               "name": "UNAUTHENTICATED"})
        rest = path[len(API):]
        if rest == "/v1/meta/info":
            return reply(200, {"applicationVersion": "6.1.0-mock"})
        if rest == "/v1/cameras":
            return reply(200, [self._camera(c) for c in CAMERAS])
        for cam in CAMERAS:
            base = f"/v1/cameras/{cam[0]}"
            if rest == base + "/snapshot":
                return 200, [("Content-Type", "image/jpeg")], self._picture(cam)
            if rest == base + "/rtsps-stream":
                quality = "high"
                if method == "POST":
                    quality = (json.loads(body or b"{}").get("qualities") or ["high"])[0]
                url = f"rtsps://{INTERNAL_HOST}:7441/{cam[0]}-{quality}?enableSrtp"
                return reply(200, {quality: url})
        return reply(404, {"error": "Resource does not exist."})

    def _camera(self, cam) -> dict:
        cam_id, name, model, kinds = cam
        return {
            "id": cam_id, "name": name, "type": model, "modelKey": "camera",
            "state": "DISCONNECTED" if cam_id in self.offline else "CONNECTED",
            "featureFlags": {"smartDetectTypes": kinds, "smartDetectAudioTypes": ["smoke"],
                             "supportFullHdSnapshot": True},
        }

    def _picture(self, cam) -> bytes:
        if self.scenes:
            scene = self.scenes / (cam[1].lower().replace(" ", "-") + ".jpg")
            if scene.is_file():
                return scene.read_bytes()
        return PIXEL

    def _login(self, body: bytes):
        now = time.time()
        self.login_failures = [t for t in self.login_failures if now - t < 60]
        if len(self.login_failures) >= LOGIN_FAILURES_ALLOWED:
            return 429, [("Retry-After", "60")], b'{"code":"AUTHENTICATION_FAILED_LIMIT_REACHED"}'
        try:
            login = json.loads(body)
        except ValueError:
            return 400, [], b""
        if login.get("password") != self.password or login.get("username") not in (USER, MFA_USER):
            self.login_failures.append(now)
            return 401, [], b'{"code":"AUTHENTICATION_FAILED_INVALID_CREDENTIALS"}'
        if login["username"] == MFA_USER:
            return 499, [], b'{"code":"MFA_AUTH_REQUIRED"}'
        token = secrets.token_hex(12)
        self.sessions.add(token)
        return 200, [("Set-Cookie", "theme=dark; Path=/"),
                     ("Set-Cookie", f"TOKEN={token}; path=/; samesite=none; secure; httponly"),
                     ("X-Updated-CSRF-Token", "csrf-" + token)], b"{}"

    def _private(self, method: str, path: str, query: dict, headers: dict):
        cookie = headers.get("cookie", "")
        token = cookie.removeprefix("TOKEN=").split(";")[0]
        if token not in self.sessions:
            return 401, [], b""
        if path == "/proxy/protect/api/ws/livestream":
            if self.livestream_fails:
                return 500, [], b""
            camera = query.get("camera", [""])[0]
            url = f"wss://{INTERNAL_HOST}:{self.port}/ws/livestream?camera={camera}&token={token}"
            return 200, [("Content-Type", "application/json")], json.dumps({"url": url}).encode()
        if path == "/proxy/protect/api/events":
            start = int(query.get("start", ["0"])[0])
            end = int(query.get("end", [str(2**62)])[0])
            limit = min(int(query.get("limit", ["100"])[0]), 200)
            events = [e for e in self.history if start <= e["start"] <= end][:limit]
            return 200, [("Content-Type", "application/json")], json.dumps(events).encode()
        if path.startswith("/proxy/protect/api/events/") and path.endswith("/thumbnail"):
            event_id = path.split("/")[-2]
            event = next((e for e in self.history if e["id"] == event_id), None)
            if not event or event["type"] not in ("smartDetectZone", "motion", "ring"):
                return 404, [], b""
            cam = next(c for c in CAMERAS if c[0] == event["device"])
            return 200, [("Content-Type", "image/jpeg")], self._picture(cam)
        return 404, [], b""

    def _control(self, method: str, path: str, query: dict):
        if path == "/mock/trigger" and method == "POST":
            camera = query.get("camera", ["Driveway"])[0]
            kind = query.get("kind", ["person"])[0]
            asyncio.get_running_loop().create_task(self.trigger(camera, kind, 2.0))
            return 202, [], b""
        if path == "/mock/expire-sessions" and method == "POST":
            self.sessions.clear()
            return 204, [], b""
        if path == "/mock/livestream" and method == "POST":
            self.livestream_fails = query.get("fail", ["true"])[0] == "true"
            return 204, [], b""
        return 404, [], b""

    # ------------------------------------------------------------------ websockets

    async def _websocket(self, head: bytes, reader, writer) -> None:
        protocol = ServerProtocol()
        protocol.receive_data(head)
        request = next((e for e in protocol.events_received() if isinstance(e, WsRequest)), None)
        if request is None:
            writer.close()
            return
        path = urlsplit(request.path).path
        self.seen.append(Seen("GET", request.path, {k.lower(): v for k, v in request.headers.raw_items()}))
        sockets = None
        if path == API + "/v1/subscribe/events":
            sockets = self._event_sockets
        elif path == API + "/v1/subscribe/devices":
            sockets = self._device_sockets
        if path.startswith(API) and request.headers.get("X-API-KEY") != self.api_key:
            response = protocol.reject(401, "Unauthorized\n")
        elif sockets is None and path != "/ws/livestream":
            response = protocol.reject(404, "Not Found\n")
        else:
            response = protocol.accept(request)
        protocol.send_response(response)
        await _flush(protocol, writer)
        if response.status_code != 101:
            writer.close()
            return
        entry = (protocol, writer)
        streaming = None
        if sockets is not None:
            sockets.append(entry)
        else:
            streaming = asyncio.get_running_loop().create_task(_stream_video(protocol, writer))
        try:
            while True:
                data = await reader.read(65536)
                if not data:
                    break
                protocol.receive_data(data)
                protocol.events_received()
                await _flush(protocol, writer)
        except (ConnectionError, ssl.SSLError, OSError):
            pass
        finally:
            if streaming:
                streaming.cancel()
            if sockets is not None and entry in sockets:
                sockets.remove(entry)
            writer.close()


async def _flush(protocol, writer) -> None:
    for chunk in protocol.data_to_send():
        if chunk:
            writer.write(chunk)
    await writer.drain()


def livestream_frame(kind: int, payload: bytes) -> bytes:
    return bytes([kind]) + len(payload).to_bytes(3, "big") + payload


async def _stream_video(protocol, writer) -> None:
    """Placeholder fMP4: the init segment once, then a segment every 100 ms."""
    try:
        protocol.send_binary(livestream_frame(250, b"ftyp-moov-init"))
        await _flush(protocol, writer)
        n = 0
        while True:
            n += 1
            segment = (livestream_frame(249, b"") + livestream_frame(251, b"moof%d" % n)
                       + livestream_frame(254, b"mdat%d" % n) + livestream_frame(255, b""))
            protocol.send_binary(segment)
            await _flush(protocol, writer)
            await asyncio.sleep(0.1)
    except (ConnectionError, OSError, asyncio.CancelledError):
        pass


def _made_up_history() -> list[dict]:
    now = int(time.time() * 1000)
    events = []
    for i in range(40):
        cam = CAMERAS[i % len(CAMERAS)]
        start = now - (40 - i) * 3_600_000
        kind = cam[3][i % len(cam[3])]
        events.append({"id": f"hist-{i}", "type": "smartDetectZone", "start": start,
                       "end": start + 12_000, "device": cam[0], "camera": cam[0],
                       "smartDetectTypes": [kind]})
    events.append({"id": "hist-admin", "type": "access", "start": now - 1000, "end": None,
                   "device": None, "smartDetectTypes": []})
    return events


def self_signed(directory: Path) -> tuple[Path, Path]:
    """A throwaway self-signed certificate and key, made with the openssl CLI."""
    cert, key = directory / "cert.pem", directory / "key.pem"
    subprocess.run(
        ["openssl", "req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:prime256v1",
         "-nodes", "-days", "2", "-subj", "/CN=unifi.mock", "-keyout", str(key), "-out", str(cert)],
        check=True, capture_output=True)
    return cert, key


def server_context(cert: Path, key: Path) -> ssl.SSLContext:
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(cert, key)
    return context


async def _main() -> None:
    parser = argparse.ArgumentParser(description="Fake UniFi Protect console")
    parser.add_argument("--port", type=int, default=7447)
    parser.add_argument("--bind", default="127.0.0.1")
    parser.add_argument("--tls", action="store_true", help="serve https with a self-signed cert")
    parser.add_argument("--interval", type=float, default=0, help="fire a detection every N s")
    parser.add_argument("--scenes", type=Path, help="folder of <camera-name>.jpg pictures")
    args = parser.parse_args()
    tls = None
    if args.tls:
        directory = Path(tempfile.mkdtemp(prefix="sauron-mock-"))
        tls = server_context(*self_signed(directory))
        os.environ.setdefault("SAURON_MOCK_CERT", str(directory / "cert.pem"))
    mock = await MockConsole(port=args.port, host=args.bind, tls=tls, scenes=args.scenes).start()
    print(f"mock console on {mock.address} (API key {API_KEY!r}, user {USER!r}/{PASSWORD!r})",
          flush=True)
    n = 0
    while True:
        if args.interval > 0:
            await asyncio.sleep(args.interval)
            cam = CAMERAS[n % len(CAMERAS)]
            n += 1
            asyncio.get_running_loop().create_task(mock.trigger(cam[0], cam[3][0], 3.0))
        else:
            await asyncio.sleep(3600)


if __name__ == "__main__":
    try:
        asyncio.run(_main())
    except KeyboardInterrupt:
        pass
