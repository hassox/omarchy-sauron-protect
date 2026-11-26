"""UniFi OS login (a local service account, no MFA) and Protect's private API: the livestream
endpoint, event history and event thumbnails.

Protocol as implemented by hjdhjd/unifi-protect and uilibs/uiprotect:
- `POST /api/auth/login` with `{username, password, rememberMe, token}`; the session is the `TOKEN`
  (or `UOS_TOKEN`) cookie plus a CSRF token from `X-Updated-CSRF-Token`, falling back to
  `X-CSRF-Token`, sent back as `X-CSRF-Token`. 401 = bad credentials, 499 or `MFA_AUTH_REQUIRED` =
  MFA, 429 = too many attempts.
- `GET /proxy/protect/api/ws/livestream?…` answers `{"url": "wss://…"}`; the URL carries its own
  token and its hostname may be internal, so it is pointed at the active console address.
- `GET /proxy/protect/api/events?start=…&end=…&limit=…&orderDirection=ASC` lists events whose start
  lies in `[start, end]` (Unix ms, inclusive), ordered by start.
- `GET /proxy/protect/api/events/<id>/thumbnail` is the event's JPEG; 404 when there is none.
"""

from __future__ import annotations

import asyncio
import json
import os
import time
from dataclasses import asdict, dataclass
from pathlib import Path
from urllib.parse import quote, urlencode, urlsplit, urlunsplit

from . import config, keyring, net
from .protect import Protect

LOGIN_PATH = "/api/auth/login"
LIVESTREAM_PATH = "/proxy/protect/api/ws/livestream"
EVENTS_PATH = "/proxy/protect/api/events"
MFA_STATUS = 499
CHUNK_SIZE = "16384"
FRAGMENT_MILLIS = "100"
# UniFi OS locks accounts after repeated failures: after a failed automatic login, wait.
LOGIN_FAILURE_PAUSE = 15 * 60
RATE_LIMIT_PAUSE = 5 * 60


class LoginError(Exception):
    """Why a login failed, worded for people. `kind`: credentials, mfa, rate, paused, other."""

    def __init__(self, kind: str, message: str, wait: float | None = None):
        self.kind, self.wait = kind, wait
        super().__init__(message)


def _bad_credentials() -> LoginError:
    return LoginError("credentials", "the console rejected the username or password")


def _mfa() -> LoginError:
    return LoginError("mfa", "the account requires MFA; use a local-only UniFi user without MFA")


def _rate_limited(wait: float | None) -> LoginError:
    if wait:
        return LoginError("rate", f"too many login attempts; the console asks to wait {int(wait)}s", wait)
    return LoginError("rate", "too many login attempts; try again in a few minutes")


@dataclass
class Session:
    host: str
    username: str
    cookie: str
    csrf: str | None = None


def session_path() -> Path:
    return config.runtime_dir() / "session"


def pause_path() -> Path:
    return config.runtime_dir() / "login-pause"


def _read_json(path: Path) -> dict | None:
    try:
        value = json.loads(path.read_bytes())
    except (OSError, ValueError):
        return None
    return value if isinstance(value, dict) else None


def write_private(path: Path, data: bytes) -> None:
    """Atomically writes a file only the user can read, creating its directory (0700)."""
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    tmp = path.with_name(path.name + ".tmp")
    fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "wb") as file:
        file.write(data)
    os.replace(tmp, path)


def _remove(path: Path) -> None:
    try:
        path.unlink()
    except OSError:
        pass  # missing is the goal; anything else only leaves a stale file for the next login


def forget_session() -> None:
    """Drops the cached session and any login pause (new config or credentials)."""
    _remove(session_path())
    _remove(pause_path())


def channel(quality: str) -> int:
    return {"medium": 1, "low": 2}.get(quality, 0)


def csrf_header(response: net.Response) -> str | None:
    return response.header("x-updated-csrf-token") or response.header("x-csrf-token")


def session_cookie(response: net.Response) -> str | None:
    """`TOKEN=…` (or `UOS_TOKEN=…`) from the login response, without cookie attributes."""
    pairs = []
    for value in response.header_all("set-cookie"):
        pair = value.split(";", 1)[0].strip()
        if "=" in pair:
            pairs.append(pair)
    for pair in pairs:
        if pair.startswith(("TOKEN=", "UOS_TOKEN=")):
            return pair
    return pairs[0] if pairs else None


def retry_after(response: net.Response) -> float | None:
    try:
        return float(int((response.header("retry-after") or "").strip()))
    except ValueError:
        return None


class PrivateApi:
    """Private-API client for one console and service account, on the active address."""

    def __init__(self, protect: Protect, username: str, password: str):
        self.protect = protect
        self.host = protect.console_id
        self.username = username
        self.password = password
        self.session: Session | None = None

    async def login(self) -> None:
        """Logs in now, ignoring any pause (the user asked), and caches the session."""
        try:
            session = await self._post_login()
        except LoginError as err:
            if err.kind in ("credentials", "mfa"):
                self._pause(LOGIN_FAILURE_PAUSE, err)
            elif err.kind == "rate":
                self._pause(max(err.wait or RATE_LIMIT_PAUSE, 1), err)
            self.session = None
            raise
        self._save(session)
        _remove(pause_path())
        self.session = session

    def _pause(self, wait: float, err: LoginError) -> None:
        pause = {"host": self.host, "username": self.username,
                 "until_ms": int((time.time() + wait) * 1000), "reason": str(err)}
        try:
            write_private(pause_path(), json.dumps(pause).encode())
        except OSError:
            pass

    def _save(self, session: Session) -> None:
        try:
            write_private(session_path(), json.dumps(asdict(session)).encode())
        except OSError:
            pass  # only costs a login next time

    async def ensure_session(self) -> Session:
        """The cached session for this console and user, else a fresh login (unless paused)."""
        if self.session is None:
            cached = _read_json(session_path())
            if cached and cached.get("host") == self.host and cached.get("username") == self.username:
                try:
                    self.session = Session(**cached)
                except TypeError:
                    self.session = None
        if self.session is None:
            await self._automatic_login()
        assert self.session is not None
        return self.session

    async def _automatic_login(self) -> None:
        """A login the user didn't ask for: honours the pause left by an earlier failure."""
        pause = _read_json(pause_path())
        if pause and pause.get("host") == self.host and pause.get("username") == self.username:
            remaining = (int(pause.get("until_ms", 0)) - time.time() * 1000) / 1000
            if remaining > 0:
                raise LoginError(
                    "paused",
                    f"not logging in again for {int(remaining)}s after: {pause.get('reason')} "
                    "(set up instant live again in the panel to retry now)",
                    remaining,
                )
        await self.login()

    async def _post_login(self) -> Session:
        body = json.dumps({"username": self.username, "password": self.password,
                           "rememberMe": True, "token": ""}).encode()

        async def send(csrf: str | None) -> net.Response:
            headers = [("Content-Type", "application/json")]
            if csrf:
                headers.append(("X-CSRF-Token", csrf))
            try:
                return await self.protect.http.request("POST", LOGIN_PATH, headers, body)
            except net.TransportError as err:
                raise LoginError("other", str(err)) from err

        response = await send(None)
        # Some UniFi OS versions want a CSRF token even for the login; the root page hands one
        # out. Definite answers aren't retried: each failed attempt counts toward the lockout.
        definite = 200 <= response.status < 300 or response.status in (401, 429, MFA_STATUS)
        if not definite:
            root = await self._root_csrf()
            if root:
                response = await send(root)
        code = None
        try:
            code = json.loads(response.body).get("code")
        except (ValueError, AttributeError):
            pass
        if response.status == MFA_STATUS or code == "MFA_AUTH_REQUIRED":
            raise _mfa()
        if response.status == 401:
            raise _bad_credentials()
        if response.status == 429:
            raise _rate_limited(retry_after(response))
        if not 200 <= response.status < 300:
            raise LoginError("other", f"login failed: HTTP {response.status}")
        cookie = session_cookie(response)
        if not cookie:
            raise LoginError("other", "login succeeded but the console sent no session cookie")
        return Session(self.host, self.username, cookie, csrf_header(response))

    async def _root_csrf(self) -> str | None:
        try:
            return csrf_header(await self.protect.http.request("GET", "/"))
        except net.TransportError:
            return None

    async def get(self, target: str) -> net.Response:
        """GETs `target` with the session, logging in again once if it has expired. 403 (no
        access to Protect) is an error."""
        relogged = False
        while True:
            session = await self.ensure_session()
            headers = [("Cookie", session.cookie)]
            if session.csrf:
                headers.append(("X-CSRF-Token", session.csrf))
            response = await self.protect.http.request("GET", target, headers)
            if response.status == 401 and not relogged:
                relogged = True
                self.session = None
                _remove(session_path())
                await self._automatic_login()
                continue
            updated = response.header("x-updated-csrf-token")
            if updated and updated != session.csrf:
                self.session = Session(session.host, session.username, session.cookie, updated)
                self._save(self.session)
            if response.status == 403:
                raise PermissionError("the UniFi user may not view Protect (give it Protect: View Only)")
            return response

    async def livestream_url(self, camera: str, channel_number: int) -> str:
        """The websocket URL of a camera's livestream, pointed at the active console address."""
        query = urlencode([
            ("allowPartialGOP", ""), ("camera", camera), ("channel", str(channel_number)),
            ("chunkSize", CHUNK_SIZE), ("fragmentDurationMillis", FRAGMENT_MILLIS), ("lens", "0"),
            ("progressive", ""), ("rebaseTimestampsToZero", "true"),
            ("requestId", f"sauron-{camera}-{channel_number}"), ("type", "fmp4"),
            ("useWallClock", "false"),
        ])
        response = await self.get(f"{LIVESTREAM_PATH}?{query}")
        if not 200 <= response.status < 300:
            raise RuntimeError(f"{LIVESTREAM_PATH}: HTTP {response.status}")
        try:
            url = json.loads(response.body)["url"]
            parts = urlsplit(url)
            if parts.scheme not in ("wss", "ws") or not parts.hostname:
                raise ValueError
        except (ValueError, KeyError, TypeError):
            raise RuntimeError(f"{LIVESTREAM_PATH}: unexpected response") from None
        # The minted URL may name an internal host; the active address is what we checked.
        netloc = self.protect.endpoint.authority if parts.port is None else (
            f"{self.protect.hostname}:{parts.port}")
        scheme = "wss" if self.protect.scheme == "https" else "ws"
        return urlunsplit((scheme, netloc, parts.path, parts.query, ""))

    async def events(self, start: int, end: int, limit: int) -> bytes:
        """Up to `limit` events (a JSON array) that started in `[start, end]`, oldest first."""
        query = urlencode([("start", start), ("end", end), ("limit", limit),
                           ("orderDirection", "ASC"), ("withoutDescriptions", "false")])
        response = await self.get(f"{EVENTS_PATH}?{query}")
        if not 200 <= response.status < 300:
            raise RuntimeError(f"{EVENTS_PATH}: HTTP {response.status}")
        return response.body

    async def event_thumbnail(self, event_id: str) -> bytes | None:
        """The event's thumbnail JPEG; None when Protect has none for it."""
        response = await self.get(f"{EVENTS_PATH}/{quote(event_id, safe='')}/thumbnail")
        if response.status == 404:
            return None
        if not 200 <= response.status < 300:
            raise RuntimeError(f"{EVENTS_PATH}/{event_id}/thumbnail: HTTP {response.status}")
        return response.body


class Warmer:
    """Keeps a private-API session cached, so clicking an alert starts video without a login
    round trip. Re-created on every config reload."""

    def __init__(self, username: str):
        self.username = username
        self._password: str | None = None
        self._looked_up = False
        self._busy = asyncio.Lock()

    async def warm(self, client: Protect) -> None:
        async with self._busy:
            if not self._looked_up:
                self._password = await keyring.lookup(client.console_id, self.username)
                self._looked_up = True
            if not self._password:
                return
            api = PrivateApi(client, self.username, self._password)
            try:
                await api.ensure_session()
            except LoginError as err:
                if err.kind != "paused":
                    raise
