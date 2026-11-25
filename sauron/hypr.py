"""Hyprland window lookup and focus through `hyprctl`."""

from __future__ import annotations

import asyncio
import json

MAP_POLL = 0.05
MAP_TIMEOUT = 5.0


async def _hyprctl(*args: str) -> tuple[int, str]:
    process = await asyncio.create_subprocess_exec(
        "hyprctl", *args, stdin=asyncio.subprocess.DEVNULL, stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE)
    stdout, stderr = await process.communicate()
    reply = stdout.decode(errors="replace").strip() or stderr.decode(errors="replace").strip()
    return process.returncode or 0, reply


async def clients() -> list[dict]:
    code, reply = await _hyprctl("clients", "-j")
    if code != 0:
        raise RuntimeError(f"hyprctl clients failed ({code})")
    try:
        windows = json.loads(reply)
    except ValueError:
        raise RuntimeError("unexpected `hyprctl clients -j` output") from None
    return [w for w in windows if isinstance(w, dict)] if isinstance(windows, list) else []


async def focus(selector: str) -> None:
    """Focuses the window matching a selector such as `pid:123` or `address:0x…` (Lua dispatcher
    on Hyprland 0.55 and later, the legacy one otherwise)."""
    _, reply = await _hyprctl("dispatch", f'hl.dsp.focus({{ window = "{selector}" }})')
    if reply == "ok":
        return
    _, legacy = await _hyprctl("dispatch", "focuswindow", selector)
    if legacy != "ok":
        raise RuntimeError(f"cannot focus window {selector}: {reply}")


async def focus_when_mapped(pid: int) -> None:
    """Waits for a window of `pid`, then focuses it: Hyprland leaves a new window unfocused while
    a layer-shell surface (the bar panel, a notification) holds the keyboard."""
    loop = asyncio.get_running_loop()
    deadline = loop.time() + MAP_TIMEOUT
    while True:
        if any(w.get("pid") == pid for w in await clients()):
            await focus(f"pid:{pid}")
            return
        if loop.time() >= deadline:
            raise RuntimeError(f"no window appeared for pid {pid} within {MAP_TIMEOUT:g}s")
        await asyncio.sleep(MAP_POLL)
