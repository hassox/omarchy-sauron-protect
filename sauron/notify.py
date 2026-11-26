"""Desktop notifications through `notify-send`, which talks to Omarchy's notification server."""

from __future__ import annotations

import asyncio
import json
import time
from collections import OrderedDict

# Notification ids remembered for in-place replacement.
REMEMBERED_IDS = 64
# Nerd Font glyph shown by the Omarchy notification daemon.
GLYPH = "\U000f0208"

LABELS = {
    "person": "Person", "vehicle": "Vehicle", "animal": "Animal", "package": "Package",
    "face": "Face", "licensePlate": "License plate", "audio": "Sound", "ring": "Doorbell",
    "motion": "Motion",
}


def label(kind: str) -> str:
    return LABELS.get(kind) or kind[:1].upper() + kind[1:]


def summary(kinds: list[str], camera: str) -> str:
    """`Person, Vehicle · Driveway`."""
    return ", ".join(label(k) for k in kinds) + " · " + camera


def local_hms(ms: int) -> str:
    return time.strftime("%H:%M:%S", time.localtime(ms // 1000))


class Notifier:
    def __init__(self, live_argv: list[str]):
        # Clicking a notification runs `<live_argv> <camera id>` (Omarchy's exec-argv hint).
        self.live_argv = live_argv
        self._ids: OrderedDict[str, int] = OrderedDict()
        self._lock = asyncio.Lock()

    def command(self, event_id: str, camera_id: str, camera_name: str, kinds: list[str],
                start_ms: int, image: str | None) -> list[str]:
        argv = [
            "notify-send", "--print-id", "--app-name=Sauron", "--urgency=normal",
            f"--hint=string:omarchy-glyph:{GLYPH}",
            f"--hint=string:omarchy-exec-argv:{json.dumps([*self.live_argv, camera_id])}",
        ]
        if image:
            argv.append(f"--hint=string:image-path:{image}")
        if replaces := self._ids.get(event_id):
            argv.append(f"--replace-id={replaces}")
        # `--` keeps a camera name that starts with a dash from reading as an option.
        return argv + ["--", summary(kinds, camera_name), local_hms(start_ms)]

    async def notify(self, event_id: str, camera_id: str, camera_name: str, kinds: list[str],
                     start_ms: int, image: str | None = None) -> None:
        """Shows the notification for an event, or updates the one already shown."""
        async with self._lock:
            argv = self.command(event_id, camera_id, camera_name, kinds, start_ms, image)
            try:
                process = await asyncio.create_subprocess_exec(
                    *argv, stdin=asyncio.subprocess.DEVNULL, stdout=asyncio.subprocess.PIPE,
                    stderr=asyncio.subprocess.PIPE)
            except OSError as err:
                raise RuntimeError(f"cannot run notify-send: {err.strerror or err}") from None
            stdout, stderr = await process.communicate()
            if process.returncode != 0:
                raise RuntimeError(f"notify-send failed: {stderr.decode(errors='replace').strip()}")
            try:
                notification_id = int(stdout.split()[0])
            except (IndexError, ValueError):
                return
            self._ids.pop(event_id, None)
            self._ids[event_id] = notification_id
            while len(self._ids) > REMEMBERED_IDS:
                self._ids.popitem(last=False)
