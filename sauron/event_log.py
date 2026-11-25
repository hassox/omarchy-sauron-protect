"""Camera events as JSON Lines: the `event_log` file the daemon appends to, and the camera lines of
`sauron log`."""

from __future__ import annotations

import asyncio
import json
import os
import time
from pathlib import Path

# The camera event types the daemon tracks.
EVENT_TYPES = ("ring", "motion", "smartDetectZone", "smartDetectLine", "smartDetectLoiterZone",
               "smartAudioDetect")


def event_kinds(event_type: str, smart_detect_types) -> list[str]:
    """Kinds of an event, normalised for filtering and display."""
    if event_type == "ring":
        return ["ring"]
    if event_type == "motion":
        return ["motion"]
    if event_type == "smartAudioDetect":
        return ["audio"]
    return [k for k in smart_detect_types or [] if isinstance(k, str)]


def rfc3339(ms: int) -> str:
    """Local time with milliseconds and the UTC offset: `2026-09-29T14:02:11.482-05:00`."""
    tm = time.localtime(ms // 1000)
    offset = tm.tm_gmtoff // 60
    sign = "-" if offset < 0 else "+"
    offset = abs(offset)
    return (time.strftime("%Y-%m-%dT%H:%M:%S", tm)
            + f".{ms % 1000:03d}{sign}{offset // 60:02d}:{offset % 60:02d}")


def duration_s(start: int, end: int) -> float:
    """Seconds from `start` to `end` (Unix ms), to a tenth of a second."""
    # Integer maths: halves round up, as people expect, with no float error (4050 ms is 4.1 s).
    return ((max(end - start, 0) + 50) // 100) / 10


def camera_line(phase: str, event_id: str, camera: str, camera_id: str, event_type: str,
                kinds: list[str], start: int, end: int | None, matched: bool | None = None,
                forced: bool = False) -> dict:
    """One camera event line; keys in this order."""
    line = {"phase": phase, "id": event_id, "camera": camera, "camera_id": camera_id,
            "type": event_type, "kinds": kinds}
    if matched is not None:
        line["match"] = matched
    line["start"] = rfc3339(start)
    line["end"] = rfc3339(end) if end is not None else None
    line["duration_s"] = duration_s(start, end) if end is not None else None
    if forced:
        line["forced"] = True
    return line


def append(path: Path, line: bytes) -> None:
    """Opens, appends one line and closes, so logrotate can move the file at any time. A line
    this short goes out in one write, which O_APPEND keeps whole."""
    flags = os.O_WRONLY | os.O_APPEND | os.O_CREAT
    try:
        fd = os.open(path, flags, 0o600)
    except FileNotFoundError:
        path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        fd = os.open(path, flags, 0o600)
    try:
        os.write(fd, line)
    finally:
        os.close(fd)


class Writer:
    """Appends lines in order on a worker thread, so a slow disk never delays an event.
    `warn` hears each distinct error once per log path."""

    def __init__(self, warn):
        self._warn = warn
        self._queue: asyncio.Queue[tuple[Path, bytes]] = asyncio.Queue(maxsize=1024)
        self._task: asyncio.Task | None = None
        self._reported: set[str] = set()
        self._current: Path | None = None

    def write(self, path: Path, line: dict) -> None:
        if self._task is None:
            self._task = asyncio.get_running_loop().create_task(self._run())
        data = (json.dumps(line, ensure_ascii=False, separators=(",", ":")) + "\n").encode()
        try:
            self._queue.put_nowait((path, data))
        except asyncio.QueueFull:
            self._report(f"event log {path}: falling behind, dropped a line")

    async def _run(self) -> None:
        while True:
            path, data = await self._queue.get()
            if path != self._current:
                self._current = path
                self._reported.clear()
            try:
                await asyncio.to_thread(append, path, data)
            except OSError as err:
                self._report(f"event log {path}: {err.strerror or err}")

    def _report(self, message: str) -> None:
        if message not in self._reported:
            self._reported.add(message)
            self._warn(message)

    async def flush(self) -> None:
        """Waits until every queued line is written (tests)."""
        while not self._queue.empty():
            await asyncio.sleep(0.01)
        await asyncio.sleep(0.02)
