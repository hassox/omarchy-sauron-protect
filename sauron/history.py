"""`sauron log` and `sauron thumbnail`: Protect's own event history and event thumbnails, read
through the private API with the instant-live account (the Integration API has no history)."""

from __future__ import annotations

import asyncio
import json
import re
import sys
import time
from datetime import datetime
from pathlib import Path

from . import event_log, keyring
from .config import Config
from .private_api import PrivateApi
from .protect import Protect

# Events per request, walking forward by start time so a week of history never sits in memory.
# A short page ends the walk, so this must not exceed any server cap (one has been seen at 200).
PAGE = 200
THUMBNAIL_WAIT = 10.0
THUMBNAIL_RETRY = 0.5
WHEN_HELP = 'use 30m, 24h, 7d, YYYY-MM-DD, "YYYY-MM-DD HH:MM" or RFC 3339'


def parse_when(text: str, now_ms: int) -> int:
    """Unix ms for `30m`, `24h`, `7d` (before now), RFC 3339, or local `YYYY-MM-DD[ HH:MM[:SS]]`."""
    text = text.strip()
    units = {"m": 60_000, "h": 3_600_000, "d": 86_400_000}
    if text[-1:] in units and text[:-1].isdigit():
        return now_ms - int(text[:-1]) * units[text[-1]]
    match = re.fullmatch(
        r"\d{4}-\d{2}-\d{2}(?:[Tt ](\d{2}):\d{2}(?::\d{2}(?:[.,]\d+)?)?)?([Zz]|[+-]\d{2}:\d{2})?", text)
    # fromisoformat reads 24:00 as the next midnight; nobody means that.
    if not match or (match.group(1) and int(match.group(1)) > 23):
        raise ValueError(f"invalid time {text!r}: {WHEN_HELP}")
    if match.group(2) == "z":
        text = text[:-1] + "Z"
    try:
        moment = datetime.fromisoformat(text.replace(",", "."))
    except ValueError:
        raise ValueError(f"invalid time {text!r}: {WHEN_HELP}") from None
    return int(moment.timestamp() * 1000)  # naive = local time


def time_range(since: str, until: str | None, now_ms: int) -> tuple[int, int]:
    start = parse_when(since, now_ms)
    end = parse_when(until, now_ms) if until else now_ms
    if start > end:
        raise ValueError(f"--since {event_log.rfc3339(start)} is after --until {event_log.rfc3339(end)}")
    return start, end


class Pager:
    """Walks the history forward a page at a time. Each page starts at the previous page's last
    start, since several events can share a millisecond; `printed_at_cursor` holds the ids already
    printed from that millisecond."""

    def __init__(self, since: int, until: int):
        self.cursor, self.until = since, until
        self.printed_at_cursor: set[str] = set()

    def is_new(self, event: dict) -> bool:
        return self.cursor <= event["start"] <= self.until and event["id"] not in self.printed_at_cursor

    def advance(self, page: list[dict], limit: int) -> bool:
        """Moves past `page`; False when it was the last one."""
        if len(page) < limit:
            return False
        last = max((e["start"] for e in page), default=self.cursor)
        if last > self.cursor:
            self.printed_at_cursor = {e["id"] for e in page if e["start"] == last}
            self.cursor = last
        else:
            # A whole page from one millisecond: step past it rather than ask for it again.
            self.printed_at_cursor = set()
            self.cursor += 1
        return True


def _valid(event) -> bool:
    return (isinstance(event, dict) and isinstance(event.get("id"), str)
            and isinstance(event.get("type"), str) and isinstance(event.get("start"), int))


def line_for(event: dict, names: dict[str, str], everything: bool) -> dict | None:
    """The event-log shape for camera events; with `everything`, a looser one for the rest."""
    end = event.get("end") if isinstance(event.get("end"), int) else None
    phase = "end" if end is not None else "start"
    camera_id = event.get("camera") if isinstance(event.get("camera"), str) else None
    camera = names.get(camera_id, camera_id) if camera_id else None
    if event["type"] in event_log.EVENT_TYPES:
        return event_log.camera_line(phase, event["id"], camera or "", camera_id or "", event["type"],
                                     event_log.event_kinds(event["type"], event.get("smartDetectTypes")),
                                     event["start"], end)
    if not everything:
        return None
    return {"phase": phase, "id": event["id"], "type": event["type"], "camera": camera,
            "camera_id": camera_id, "start": event_log.rfc3339(event["start"]),
            "end": event_log.rfc3339(end) if end is not None else None,
            "duration_s": event_log.duration_s(event["start"], end) if end is not None else None,
            "details": event.get("metadata")}


async def _service_password(config: Config, client: Protect, command: str) -> str:
    password = await keyring.lookup(client.console_id, config.username) if config.username else None
    if not password:
        raise RuntimeError(f"sauron {command} reads Protect's history, which needs the instant-live "
                           "account: set up instant live in the Sauron panel")
    return password


async def log(config: Config, client: Protect, since: str, until: str | None, everything: bool) -> None:
    """Prints events from `since` to `until` as JSON Lines, oldest first, a page at a time."""
    start, end = time_range(since, until, int(time.time() * 1000))
    password = await _service_password(config, client, "log")
    names = {c.id: c.display_name for c in await client.cameras()}
    api = PrivateApi(client, config.username, password)
    pager = Pager(start, end)
    while True:
        try:
            page = json.loads(await api.events(pager.cursor, end, PAGE))
        except ValueError:
            raise RuntimeError("Protect's event list: unexpected response") from None
        page = [e for e in page if _valid(e)] if isinstance(page, list) else []
        lines = [line_for(e, names, everything) for e in page if pager.is_new(e)]
        text = "".join(json.dumps(line, ensure_ascii=False, separators=(",", ":")) + "\n"
                       for line in lines if line)
        try:
            sys.stdout.write(text)
            sys.stdout.flush()
        except BrokenPipeError:
            return  # `| head`
        if not pager.advance(page, PAGE):
            return


async def thumbnail(config: Config, client: Protect, event_id: str, out: str | None) -> None:
    """Saves the event's thumbnail JPEG and prints its path. Protect answers 404 for about a second
    after an event ends while it swaps in the final picture, so a 404 is retried for a while."""
    event_id = event_id.removeprefix("e-")  # events carry their thumbnail id as e-<id>
    if not re.fullmatch(r"[A-Za-z0-9-]+", event_id):
        raise ValueError(f"invalid event id {event_id!r}")
    path = Path(out) if out else Path(f"./{event_id}.jpg")
    api = PrivateApi(client, config.username, await _service_password(config, client, "thumbnail"))
    loop = asyncio.get_running_loop()
    deadline = loop.time() + THUMBNAIL_WAIT
    while True:
        jpeg = await api.event_thumbnail(event_id)
        if jpeg is not None:
            break
        if loop.time() + THUMBNAIL_RETRY >= deadline:
            raise RuntimeError(f"Protect has no thumbnail for event {event_id}")
        await asyncio.sleep(THUMBNAIL_RETRY)
    path.write_bytes(jpeg)
    print(path)
