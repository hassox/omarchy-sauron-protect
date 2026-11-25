"""UniFi Protect Integration API client: REST calls and websocket subscriptions."""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from urllib.parse import parse_qsl, urlencode, urlsplit, urlunsplit

from . import config as config_mod
from . import net
from .config import Config

REQUEST_TIMEOUT = 20.0
LIVENESS_TIMEOUT = 3.0
SNAPSHOT_TIMEOUT = 6.0
CONNECT_TIMEOUT = 3.0
SUMMARY_MAX = 120
PIN_HINT = "Pin your console's certificate: open the Sauron panel and set it up again"


class AuthError(Exception):
    def __str__(self) -> str:
        return "API key rejected (401)"


class HttpError(Exception):
    def __init__(self, path: str, status: int, message: str = ""):
        self.path, self.status, self.message = path, status, message
        super().__init__(f"{path}: HTTP {status}{message}")


class BadResponse(Exception):
    """The console answered with something that isn't the expected JSON."""


@dataclass
class Camera:
    id: str
    name: str | None = None
    state: str | None = None
    model: str | None = None
    feature_flags: dict = field(default_factory=dict)

    @classmethod
    def from_json(cls, item: dict) -> Camera:
        flags = item.get("featureFlags")
        return cls(
            id=str(item.get("id", "")),
            name=item.get("name") if isinstance(item.get("name"), str) else None,
            state=item.get("state") if isinstance(item.get("state"), str) else None,
            model=item.get("type") if isinstance(item.get("type"), str) else None,
            feature_flags=flags if isinstance(flags, dict) else {},
        )

    @property
    def display_name(self) -> str:
        for candidate in (self.name, self.model):
            if candidate and candidate.strip():
                return candidate
        return self.id

    @property
    def online(self) -> bool:
        return self.state == "CONNECTED"

    @property
    def full_hd_snapshot(self) -> bool:
        """Whether `highQuality=true` snapshots work (Protect answers 400 without it)."""
        return self.feature_flags.get("supportFullHdSnapshot") is True

    def kinds(self) -> list[str]:
        """Detection kinds this camera can produce."""
        kinds = [k for k in self.feature_flags.get("smartDetectTypes") or [] if isinstance(k, str)]
        if self.feature_flags.get("smartDetectAudioTypes"):
            kinds.append("audio")
        kinds.append("motion")
        if "doorbell" in (self.model or "").lower():
            kinds.append("ring")
        return kinds


def sorted_cameras(cameras: list[Camera]) -> list[Camera]:
    return sorted(cameras, key=lambda c: (c.display_name.lower(), c.id))


def resolve(cameras: list[Camera], query: str) -> Camera:
    """A camera by id, case-insensitive name, or unique case-insensitive name prefix."""
    for camera in cameras:
        if camera.id == query:
            return camera
    lowered = query.lower()
    quoted = json.dumps(lowered, ensure_ascii=False)
    for camera in cameras:
        if camera.display_name.lower() == lowered:
            return camera
    matches = [c for c in cameras if c.display_name.lower().startswith(lowered)]
    if len(matches) == 1:
        return matches[0]
    if not matches:
        raise LookupError(f"no camera matches {quoted}")
    names = ", ".join(c.display_name for c in matches)
    raise LookupError(f"{quoted} is ambiguous: {names}")


class Protect:
    """Client for one address of the console. A session's REST calls, websockets and stream URLs
    all go to this address; the console's identity stays the configured `host`."""

    def __init__(self, config: Config, address: str):
        self.config = config
        self.address = address
        self.base = config_mod.base_url(address)
        parts = urlsplit(self.base)
        self.scheme = parts.scheme
        self.hostname = parts.hostname or ""
        self.endpoint = net.Endpoint.of(self.base)
        self.console_id = config_mod.console_id(config.host)
        self.fallback = address != config.host
        self.http = net.HttpClient(self.endpoint, config.pin, CONNECT_TIMEOUT)

    @property
    def via(self) -> str | None:
        return self.address if self.fallback else None

    @property
    def security(self) -> str:
        if self.scheme == "http":
            return "no TLS"
        return "certificate pinned" if self.config.pin else "certificate trusted"

    @property
    def origin(self) -> str:
        """`scheme://host[:port]` for the UniFi OS (non-integration) endpoints."""
        parts = urlsplit(self.base)
        return urlunsplit((parts.scheme, parts.netloc, "", "", ""))

    @property
    def api_path(self) -> str:
        return urlsplit(self.base).path

    def _key_header(self) -> list[tuple[str, str]]:
        return [("X-API-KEY", self.config.api_key)] if self.config.api_key else []

    async def probe(self) -> int:
        """Status of an unauthenticated meta/info: 401 means a Protect console answered."""
        response = await self.http.request("GET", self.api_path + "/v1/meta/info",
                                           timeout=LIVENESS_TIMEOUT * 2)
        return response.status

    async def alive(self) -> None:
        await self._request("GET", "/v1/meta/info", timeout=LIVENESS_TIMEOUT)

    async def _request(self, method: str, path: str, body: dict | None = None,
                       timeout: float = REQUEST_TIMEOUT) -> bytes:
        headers = self._key_header()
        data = None
        if body is not None:
            data = json.dumps(body).encode()
            headers = headers + [("Content-Type", "application/json")]
        response = await self.http.request(method, self.api_path + path, headers, data, timeout)
        if response.status == 401:
            raise AuthError()
        if not 200 <= response.status < 300:
            raise HttpError(path.split("?")[0], response.status, _api_error(response.body))
        return response.body

    async def _json(self, path: str, body: dict | None = None, method: str = "GET"):
        data = await self._request(method, path, body)
        try:
            return json.loads(data)
        except ValueError:
            raise BadResponse(f"{path}: unexpected response") from None

    async def version(self) -> str:
        """Protect's version; also proves the key."""
        info = await self._json("/v1/meta/info")
        if not isinstance(info, dict) or not isinstance(info.get("applicationVersion"), str):
            raise BadResponse("/v1/meta/info: unexpected response")
        return info["applicationVersion"]

    async def cameras(self) -> list[Camera]:
        items = await self._json("/v1/cameras")
        if not isinstance(items, list):
            raise BadResponse("/v1/cameras: unexpected response")
        return [Camera.from_json(item) for item in items if isinstance(item, dict) and item.get("id")]

    async def snapshot(self, camera: str, high_quality: bool = False) -> bytes:
        flag = "true" if high_quality else "false"
        return await self._request("GET", f"/v1/cameras/{camera}/snapshot?highQuality={flag}",
                                   timeout=SNAPSHOT_TIMEOUT)

    async def stream_url(self, camera: str, quality: str) -> str:
        """The RTSPS URL for `quality`, creating the stream when it doesn't exist yet."""
        path = f"/v1/cameras/{camera}/rtsps-stream"
        streams = await self._json(path)
        if isinstance(streams, dict) and isinstance(streams.get(quality), str):
            return streams[quality]
        created = await self._json(path, {"qualities": [quality]}, method="POST")
        if isinstance(created, dict) and isinstance(created.get(quality), str):
            return created[quality]
        raise BadResponse(f"console returned no {quality} stream")

    async def subscribe(self, topic: str) -> net.WebSocket:
        scheme = "wss" if self.scheme == "https" else "ws"
        url = f"{scheme}://{self.endpoint.authority}{self.api_path}/v1/subscribe/{topic}"
        try:
            return await net.WebSocket.connect(url, self.config.pin, self._key_header())
        except net.WsHttpError as err:
            if err.status == 401:
                raise AuthError() from None
            raise

    async def close(self) -> None:
        await self.http.close()


def _api_error(body: bytes) -> str:
    try:
        error = json.loads(body).get("error")
    except (ValueError, AttributeError):
        return ""
    return f": {error}" if isinstance(error, str) else ""


def fixup_stream_url(url: str, hostname: str) -> str:
    """Drops `enableSrtp` and points the URL at the console, keeping its port. The URL goes
    straight to the player, which would open anything, so only RTSP(S) is accepted."""
    parts = urlsplit(url)
    if parts.scheme not in ("rtsps", "rtsp") or not parts.hostname:
        raise ValueError("stream URL must be rtsps:// or rtsp://")
    host = f"[{hostname}]" if ":" in hostname else hostname
    netloc = f"{host}:{parts.port}" if parts.port else host
    query = [(k, v) for k, v in parse_qsl(parts.query, keep_blank_values=True) if k != "enableSrtp"]
    return urlunsplit((parts.scheme, netloc, parts.path, urlencode(query), ""))


# ------------------------------------------------------------------------------ failures


def outcome(err: BaseException) -> str:
    """What happened at one address, in words short enough for the bar."""
    if isinstance(err, AuthError):
        return "API key rejected"
    if isinstance(err, net.Rejection):
        return "not your console (certificate mismatch)" if err.kind == "mismatch" else "untrusted certificate"
    if isinstance(err, net.TransportError):
        message = str(err)
        return "TLS handshake failed" if message.startswith("TLS handshake failed") else "no answer"
    if isinstance(err, HttpError):
        return f"HTTP {err.status}"
    if isinstance(err, BadResponse):
        return "unexpected response"
    return str(err) or type(err).__name__


def truncate(text: str, limit: int = SUMMARY_MAX) -> str:
    return text if len(text) <= limit else text[: limit - 1] + "…"


@dataclass
class Attempt:
    address: str
    error: BaseException

    @property
    def outcome(self) -> str:
        return outcome(self.error)


class ConnectError(Exception):
    """Every address failed. `failure` is "auth", "untrusted" or "offline"."""

    def __init__(self, attempts: list[Attempt]):
        self.attempts = attempts
        super().__init__(self.message)

    @property
    def failure(self) -> str:
        if any(isinstance(a.error, AuthError) for a in self.attempts):
            return "auth"
        if any(isinstance(a.error, net.Rejection) and a.error.kind == "untrusted" for a in self.attempts):
            return "untrusted"
        return "offline"

    def summary(self) -> str:
        return truncate(" · ".join(f"{a.address}: {a.outcome}" for a in self.attempts))

    @property
    def message(self) -> str:
        return {"auth": str(AuthError()), "untrusted": PIN_HINT}.get(self.failure) or self.summary()


def failure_message(address: str, err: BaseException) -> str:
    return truncate(f"{address}: {outcome(err)}")


async def connect(config: Config) -> tuple[Protect, str]:
    """Tries `host`, then each fallback; the first that answers meta/info with the key is the
    session's address. A rejected key stops the search."""
    attempts = []
    for address in config.addresses():
        client = None
        try:
            client = Protect(config, address)
            return client, await client.version()
        except (net.TransportError, AuthError, HttpError, BadResponse, ValueError) as err:
            if client:
                await client.close()
            attempts.append(Attempt(address, err))
            if isinstance(err, AuthError):
                break
    raise ConnectError(attempts)
