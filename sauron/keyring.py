"""The instant-live password, kept in the desktop keyring through `secret-tool`."""

from __future__ import annotations

import asyncio

# A locked keyring may prompt; don't wait on it forever.
TIMEOUT = 5.0


def _attributes(console: str, username: str) -> list[str]:
    return ["application", "sauron", "host", console, "username", username]


async def lookup(console: str, username: str) -> str | None:
    """The stored password, or None when the keyring is unavailable or has no entry."""
    try:
        process = await asyncio.create_subprocess_exec(
            "secret-tool", "lookup", *_attributes(console, username),
            stdin=asyncio.subprocess.DEVNULL,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.DEVNULL,
        )
    except OSError:
        return None
    try:
        async with asyncio.timeout(TIMEOUT):
            stdout, _ = await process.communicate()
    except TimeoutError:
        process.kill()
        await process.wait()
        return None
    password = stdout.decode(errors="replace").removesuffix("\n")
    return password if process.returncode == 0 and password else None


async def store(console: str, username: str, password: str) -> None:
    """Saves the password; it goes to secret-tool on stdin, never on its command line."""
    try:
        process = await asyncio.create_subprocess_exec(
            "secret-tool", "store", f"--label=Sauron · {console}", *_attributes(console, username),
            stdin=asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.DEVNULL,
            stderr=asyncio.subprocess.PIPE,
        )
    except OSError as err:
        raise RuntimeError(f"cannot run secret-tool: {err.strerror or err}") from None
    try:
        async with asyncio.timeout(TIMEOUT):
            _, stderr = await process.communicate(password.encode())
    except TimeoutError:
        process.kill()
        await process.wait()
        raise RuntimeError("secret-tool timed out (is the keyring unlocked?)") from None
    if process.returncode != 0:
        detail = stderr.decode(errors="replace").strip()
        raise RuntimeError(f"secret-tool store failed: {detail or process.returncode}")
