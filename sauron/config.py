"""The config file, `~/.config/sauron/config.toml`: reading, validating and writing it."""

from __future__ import annotations

import asyncio
import ipaddress
import json
import os
import struct
import tomllib
from dataclasses import dataclass, field, replace
from pathlib import Path
from urllib.parse import urlsplit, urlunsplit

from . import net

API_PATH = "/proxy/protect/integration"
DEFAULT_PLAYER = ["mpv", "--profile=low-latency", "--untimed", "--no-cache", "--force-window=immediate"]
QUALITIES = ("high", "medium", "low")
NOT_SET_UP = "Not set up yet: open the Sauron panel to connect your console"


class NotSetUp(Exception):
    """No config file, or no API key."""

    def __str__(self) -> str:
        return NOT_SET_UP


class Invalid(Exception):
    """The config can't be used; the message is shown as is."""

    def __str__(self) -> str:
        return f"Config error: {self.args[0]}"


@dataclass
class Settings:
    """The config file as written, before validation. Unknown keys are ignored."""

    host: str = ""
    fallback_hosts: list[str] = field(default_factory=list)
    cert_sha256: str = ""
    api_key: str = ""
    api_key_command: str = ""
    username: str = ""
    live_quality: str = "high"
    player: list[str] = field(default_factory=lambda: list(DEFAULT_PLAYER))
    event_log: str = ""

    @classmethod
    def from_toml(cls, data: dict) -> Settings:
        settings = cls()
        for name in cls.__dataclass_fields__:
            if name not in data:
                continue
            value = data[name]
            expected = type(getattr(settings, name))
            if expected is list:
                if not isinstance(value, list) or not all(isinstance(v, str) for v in value):
                    raise Invalid(f"`{name}` must be a list of strings")
            elif not isinstance(value, str):
                raise Invalid(f"`{name}` must be a string")
            setattr(settings, name, value)
        return settings


@dataclass(frozen=True)
class Config:
    """Validated settings with the API key resolved."""

    host: str
    fallback_hosts: tuple[str, ...]
    pin: bytes | None
    api_key: str
    username: str = ""
    live_quality: str = "high"
    player: tuple[str, ...] = tuple(DEFAULT_PLAYER)
    event_log: Path | None = None

    def addresses(self) -> list[str]:
        return [self.host, *self.fallback_hosts]


def _env_path(key: str) -> Path | None:
    value = os.environ.get(key)
    return Path(value) if value else None


def config_path() -> Path:
    if path := _env_path("SAURON_CONFIG"):
        return path
    base = _env_path("XDG_CONFIG_HOME") or (_env_path("HOME") or Path("/")) / ".config"
    return base / "sauron" / "config.toml"


def runtime_dir() -> Path:
    if path := _env_path("SAURON_RUNTIME_DIR"):
        return path
    if path := _env_path("XDG_RUNTIME_DIR"):
        return path / "sauron"
    return Path(f"/tmp/sauron-{os.getuid()}")


def address(text: str) -> str:
    """A console address as configured: trimmed, without a trailing slash."""
    return text.strip().rstrip("/")


def base_url(host: str) -> str:
    """`https://<host>/proxy/protect/integration`, or a given http(s) URL (path defaulting to the
    API path)."""
    explicit = "://" in host
    parts = urlsplit(host if explicit else f"https://{host}")
    if parts.scheme not in ("http", "https"):
        raise ValueError("host URL must be http:// or https://")
    try:
        parts.port  # noqa: B018  (raises on a bad port)
    except ValueError as err:
        raise ValueError(f"invalid host {host!r}: {err}") from None
    if not parts.hostname or any(c in parts.hostname for c in " /?#@"):
        raise ValueError(f"invalid host {host!r}")
    if parts.username or (not explicit and (parts.path or parts.query)):
        raise ValueError(f"invalid host {host!r}")
    path = parts.path if parts.path not in ("", "/") else API_PATH
    return urlunsplit((parts.scheme, parts.netloc, path.rstrip("/"), "", ""))


def console_id(host: str) -> str:
    """`host[:port]` of the configured host: the keyring and session-cache identity."""
    parts = urlsplit(base_url(host))
    return f"{parts.hostname}:{parts.port}" if parts.port else str(parts.hostname)


def read_settings(path: Path) -> Settings | None:
    """The config file, or None when it doesn't exist."""
    try:
        text = path.read_text()
    except FileNotFoundError:
        return None
    except OSError as err:
        raise Invalid(f"cannot read the file: {err.strerror or err}") from None
    try:
        return Settings.from_toml(tomllib.loads(text))
    except tomllib.TOMLDecodeError as err:
        raise Invalid(str(err).replace("\n", " ")) from None


async def load(path: Path) -> Config:
    settings = read_settings(path)
    if settings is None:
        raise NotSetUp()
    return await validate(settings)


async def validate(settings: Settings) -> Config:
    """Checks the values and resolves the API key."""
    host = address(settings.host)
    if not host:
        raise Invalid("`host` is empty")
    fallbacks = tuple(a for a in (address(h) for h in settings.fallback_hosts) if a)
    for candidate in (host, *fallbacks):
        try:
            base_url(candidate)
        except ValueError as err:
            raise Invalid(str(err)) from None
    pin = None
    if settings.cert_sha256.strip():
        pin = net.parse_fingerprint(settings.cert_sha256)
        if pin is None:
            raise Invalid("`cert_sha256` must be a SHA-256 fingerprint (64 hex digits)")
    if settings.live_quality not in QUALITIES:
        raise Invalid(f"`live_quality` must be high, medium or low, not {settings.live_quality!r}")
    if not settings.player or not settings.player[0]:
        raise Invalid("`player` must be a non-empty list")
    key, command = settings.api_key.strip(), settings.api_key_command.strip()
    if key and command:
        raise Invalid("set only one of `api_key` and `api_key_command`")
    if not key and not command:
        raise NotSetUp()
    if command:
        key = await _run_key_command(command)
    event_log = None
    if log := settings.event_log.strip():
        if log.startswith("~/"):
            home = os.environ.get("HOME")
            if not home:
                raise Invalid("`event_log` starts with ~/ but HOME is not set")
            event_log = Path(home) / log[2:]
        else:
            event_log = Path(log)
        if not event_log.is_absolute():
            raise Invalid(f"`event_log` must be an absolute path or start with ~/, not {log!r}")
    return Config(
        host=host,
        fallback_hosts=fallbacks,
        pin=pin,
        api_key=key,
        username=settings.username.strip(),
        live_quality=settings.live_quality,
        player=tuple(settings.player),
        event_log=event_log,
    )


async def _run_key_command(command: str) -> str:
    try:
        process = await asyncio.create_subprocess_exec(
            "sh", "-c", command,
            stdin=asyncio.subprocess.DEVNULL,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        stdout, stderr = await process.communicate()
    except OSError as err:
        raise Invalid(f"cannot run `api_key_command`: {err}") from None
    if process.returncode != 0:
        detail = stderr.decode(errors="replace").strip()
        raise Invalid(f"`api_key_command` failed (exit {process.returncode})"
                      + (f": {detail}" if detail else ""))
    key = stdout.decode(errors="replace").strip()
    if not key:
        raise Invalid("`api_key_command` printed nothing")
    return key


def _string(value: str) -> str:
    # A JSON string is a valid TOML basic string: the same escapes, \uXXXX for control characters.
    return json.dumps(value, ensure_ascii=False)


def _array(values: list[str]) -> str:
    return "[" + ", ".join(_string(v) for v in values) + "]"


def render(settings: Settings) -> str:
    """The commented config file with `settings` filled in."""
    pin = net.parse_fingerprint(settings.cert_sha256)
    cert = net.fingerprint_text(pin) if pin else settings.cert_sha256.strip()
    fallbacks = [a for a in (address(h) for h in settings.fallback_hosts) if a]
    command = settings.api_key_command.strip()
    key_command = f"api_key_command = {_string(command)}" if command else '# api_key_command = ""'
    return f"""\
# Written by the Sauron panel; edit it by hand if you like.
# UniFi console address: IP or hostname (https assumed). Also its identity for the keyring.
# A full URL (http://host:port) is for testing only: plain http has no TLS.
host = {_string(address(settings.host))}
# Other addresses of the same console, tried in order after host, e.g. ["100.101.102.103", "unifi.tail1234.ts.net"]
fallback_hosts = {_array(fallbacks)}
# SHA-256 of the console's certificate as openssl prints it; the panel fills it in.
# Only that certificate is accepted, at every address. Empty = require a publicly trusted certificate.
cert_sha256 = {_string(cert)}
# Protect > Settings > Control Plane > Integrations > Create API key
api_key = {_string(settings.api_key.strip())}
# Alternative to api_key: a command whose stdout is the key, e.g. "secret-tool lookup service sauron"
{key_command}
# Local UniFi OS user for instant live video; its password lives in the keyring. Empty = RTSPS only.
username = {_string(settings.username.strip())}
# Live view quality: high | medium | low
live_quality = {_string(settings.live_quality)}
# Player argv; "--title=<Sauron · camera>" (mpv only) and the stream are added.
player = {_array(settings.player)}
# Append every camera event to this file as JSON Lines (off when empty). Rotate it however you like.
event_log = {_string(settings.event_log.strip())}
"""


def save(path: Path, settings: Settings) -> None:
    """Atomically writes the config, readable only by the user (it holds the API key)."""
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_name(path.name + ".tmp")
    fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w") as file:
        file.write(render(settings))
        file.flush()
        os.fsync(file.fileno())
    os.replace(tmp, path)


def default_gateway() -> str | None:
    """The IPv4 default gateway, from /proc/net/route."""
    try:
        return gateway_from_routes(Path("/proc/net/route").read_text())
    except OSError:
        return None


def gateway_from_routes(table: str) -> str | None:
    for line in table.splitlines()[1:]:
        fields = line.split()
        if len(fields) < 3 or fields[1] != "00000000":
            continue
        try:
            gateway = int(fields[2], 16)
        except ValueError:
            continue
        if gateway:
            return str(ipaddress.IPv4Address(struct.pack("<I", gateway)))
    return None


def with_key(settings: Settings, key: str) -> Settings:
    """A copy using `key` as the API key (dropping any key command)."""
    return replace(settings, api_key=key, api_key_command="")
