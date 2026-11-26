"""Connecting Sauron to a console from the panel: the `setup.*` commands.

`load` prefills the form, `probe` finds the console at an address and shows its certificate,
`save` checks everything (API key, other addresses, instant-live login) and writes the config.
Replies never contain the API key or the password.
"""

from __future__ import annotations

import asyncio
from dataclasses import replace
from pathlib import Path

from . import config as config_mod, keyring, net, private_api, protect
from .config import Config, Settings

KEY_HINT = "Protect › Settings › Control Plane › Integrations › Create API key"
INSTANT_HINT = ("Instant live video needs a local UniFi user: UniFi OS › Admins & Users › Add, "
                "Restrict to local access only, Protect: View Only.")


def _probe_config(host: str, pin: bytes | None, api_key: str = "") -> Config:
    return Config(host=host, fallback_hosts=(), pin=pin, api_key=api_key)


async def describe_certificate(der: bytes) -> tuple[str | None, bool]:
    """The certificate's common name and whether it is self-issued, via the openssl CLI; nothing
    when openssl is missing."""
    try:
        process = await asyncio.create_subprocess_exec(
            "openssl", "x509", "-inform", "DER", "-noout", "-subject", "-issuer", "-nameopt", "RFC2253",
            stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.DEVNULL)
        stdout, _ = await asyncio.wait_for(process.communicate(der), 5)
    except (OSError, TimeoutError):
        return None, False
    lines = dict(line.split("=", 1) for line in stdout.decode(errors="replace").splitlines() if "=" in line)
    subject, issuer = lines.get("subject", ""), lines.get("issuer", "")
    common_name = None
    for part in subject.split(","):
        if part.strip().startswith("CN="):
            common_name = part.strip()[3:]
    return common_name, bool(subject) and subject == issuer


async def find_console(host: str) -> tuple[int, bytes | None, bool]:
    """(status of an unauthenticated meta/info, the leaf to pin if the certificate isn't publicly
    trusted, whether the address is plain http)."""
    trusted = protect.Protect(_probe_config(host, None), host)
    try:
        return await trusted.probe(), None, trusted.scheme == "http"
    except net.Rejection as err:
        if err.kind != "untrusted":
            raise
        leaf = err.leaf
    finally:
        await trusted.close()
    pinned = protect.Protect(_probe_config(host, net.sha256(leaf)), host)
    try:
        return await pinned.probe(), leaf, False
    finally:
        await pinned.close()


async def reach(probe: Config, address: str) -> str:
    """Whether a fallback address leads to the same console: console, silent, different,
    untrusted, or the error."""
    try:
        client = protect.Protect(probe, address)
    except ValueError as err:
        return str(err)
    try:
        await client.probe()
        return "console"
    except net.Rejection as err:
        return "different" if err.kind == "mismatch" else "untrusted"
    except net.TransportError:
        return "silent"
    finally:
        await client.close()


class Onboarding:
    def __init__(self, path: Path, saved):
        self.path = path
        self.saved = saved  # awaited after a successful save, to reconnect
        self._lock = asyncio.Lock()

    def _settings(self) -> Settings:
        try:
            return config_mod.read_settings(self.path) or Settings()
        except config_mod.Invalid:
            return Settings()

    async def handle(self, op: str, command: dict) -> dict:
        async with self._lock:
            try:
                if op == "load":
                    return await self.load()
                if op == "probe":
                    return await self.probe(str(command.get("host", "")))
                if op == "save":
                    return await self.save(command)
                return {"ok": False, "message": f"unknown setup step {op!r}"}
            except Exception as err:  # noqa: BLE001  shown in the panel instead of crashing
                return {"ok": False, "message": str(err) or type(err).__name__}

    async def load(self) -> dict:
        settings = self._settings()
        host = config_mod.address(settings.host) or config_mod.default_gateway() or ""
        pin = net.parse_fingerprint(settings.cert_sha256)
        has_password = False
        if settings.username.strip() and settings.host.strip():
            try:
                console = config_mod.console_id(settings.host)
                has_password = await keyring.lookup(console, settings.username.strip()) is not None
            except ValueError:
                pass
        return {
            "ok": True,
            "host": host,
            "configured": bool(settings.host.strip()),
            "pin": net.fingerprint_text(pin) if pin else "",
            "hasKey": bool(settings.api_key.strip() or settings.api_key_command.strip()),
            "fallbacks": [config_mod.address(h) for h in settings.fallback_hosts if h.strip()],
            "username": settings.username.strip(),
            "hasPassword": has_password,
            "keyHint": KEY_HINT,
            "instantHint": INSTANT_HINT,
        }

    async def probe(self, host: str) -> dict:
        host = config_mod.address(host)
        if not host:
            return {"ok": False, "field": "host", "message": "Enter your console's address"}
        try:
            config_mod.base_url(host)
        except ValueError as err:
            return {"ok": False, "field": "host", "message": str(err)}
        try:
            status, leaf, plain = await find_console(host)
        except net.TransportError as err:
            return {"ok": False, "field": "host", "message": f"Cannot reach {host}: {err}"}
        reply = {"ok": True, "host": host, "console": status == 401, "status": status,
                 "plain": plain, "cert": None, "changed": False}
        if status != 401:
            reply["message"] = f"{host} answered HTTP {status}; it doesn't look like a UniFi console"
        if leaf is not None:
            digest = net.sha256(leaf)
            common_name, self_signed = await describe_certificate(leaf)
            old = net.parse_fingerprint(self._settings().cert_sha256)
            reply["cert"] = {"sha256": net.fingerprint_text(digest),
                             "short": net.fingerprint_short(digest),
                             "selfSigned": self_signed, "cn": common_name}
            reply["changed"] = old is not None and old != digest
        return reply

    async def save(self, command: dict) -> dict:
        settings = self._settings()
        host = config_mod.address(str(command.get("host", "")))
        pin_text = str(command.get("pin") or "")
        pin = net.parse_fingerprint(pin_text) if pin_text else None
        if pin_text and pin is None:
            return {"ok": False, "field": "host", "message": "Find the console again"}
        api_key = str(command.get("apiKey") or "").strip()
        same_console = config_mod.address(settings.host) == host
        candidate = replace(settings, host=host,
                            cert_sha256=net.fingerprint_text(pin) if pin else "", fallback_hosts=[])
        if api_key:
            candidate = config_mod.with_key(candidate, api_key)
        elif not same_console or not (settings.api_key.strip() or settings.api_key_command.strip()):
            return {"ok": False, "field": "key", "message": "Paste an API key"}

        # The key: reach the console with it and list the cameras.
        try:
            checked = await config_mod.validate(candidate)
        except config_mod.NotSetUp:
            return {"ok": False, "field": "key", "message": "Paste an API key"}
        except config_mod.Invalid as err:
            return {"ok": False, "field": "host", "message": str(err)}
        client = protect.Protect(checked, host)
        try:
            version = await client.version()
            cameras = await client.cameras()
        except protect.AuthError:
            await client.close()
            return {"ok": False, "field": "key", "message": "The console rejected that API key"}
        except Exception as err:  # noqa: BLE001
            await client.close()
            return {"ok": False, "field": "host", "message": f"{host}: {protect.outcome(err)}"}

        try:
            # Other addresses: each must reach this same console, or be silent for now.
            fallbacks: list[str] = []
            for raw in command.get("fallbacks") or []:
                address = config_mod.address(str(raw))
                if address and address != host and address not in fallbacks:
                    fallbacks.append(address)
            probe = _probe_config(host, pin)
            results = await asyncio.gather(*(reach(probe, a) for a in fallbacks))
            for address, result in zip(fallbacks, results):
                if result in ("console", "silent"):
                    continue
                reason = {"different": "is a different machine",
                          "untrusted": "has an untrusted certificate"}.get(result, result)
                return {"ok": False, "field": "fallbacks", "message": f"{address} {reason}"}
            candidate.fallback_hosts = fallbacks

            # Instant live: log in with the local user, then keep the password in the keyring.
            username = str(command.get("username") or "").strip()
            password = str(command.get("password") or "")
            if username:
                console = config_mod.console_id(host)
                if not password:
                    password = await keyring.lookup(console, username) or ""
                if not password:
                    return {"ok": False, "field": "live", "message": "Enter the UniFi user's password"}
                api = private_api.PrivateApi(client, username, password)
                try:
                    await api.login()
                except private_api.LoginError as err:
                    return {"ok": False, "field": "live", "message": str(err).capitalize()}
                if command.get("password"):
                    try:
                        await keyring.store(console, username, password)
                    except RuntimeError as err:
                        return {"ok": False, "field": "live", "message": str(err)}
            candidate.username = username
        finally:
            await client.close()

        config_mod.save(self.path, candidate)
        private_api.forget_session()
        await self.saved()
        return {"ok": True, "version": version, "cameras": len(cameras),
                "message": f"Protect {version} · {len(cameras)} cameras"}
