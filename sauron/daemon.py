"""`sauron watch`: the long-running bridge between Protect and the shell plugin.

It speaks JSON lines with Service.qml: messages out on stdout, commands in on stdin."""

from __future__ import annotations

import asyncio
import json
import os
import signal
import sys
import time
from collections import OrderedDict
from dataclasses import dataclass, field
from pathlib import Path

from . import PROTOCOL, VERSION, config as config_mod, event_log, net, onboarding, private_api, protect
from .config import Config
from .notify import Notifier

CONFIG_POLL = 3.0
SWEEP_EVERY = 60.0
EVENT_TTL = 30 * 60.0
PING_EVERY = 20.0
DEAD_AFTER = 45.0
WATCHDOG_TICK = 5.0
BACKOFF_MIN = 2.0
BACKOFF_MAX = 30.0
AUTH_BACKOFF = 60.0
# Route changes arrive in bursts; wait for this much quiet before acting on them.
NETWORK_SETTLE = 2.0
EVENT_IMAGES_KEPT = 50
ENDED_REMEMBERED = 256
DEFAULT_NOTIFY = ["person", "vehicle", "package", "ring"]


def now_ms() -> int:
    return int(time.time() * 1000)


def _safe_id(value: str) -> bool:
    """Camera and event ids go into file names."""
    return bool(value) and all(c.isascii() and (c.isalnum() or c in "-_") for c in value)


# ------------------------------------------------------------------------------- events


@dataclass
class Filter:
    notify: list[str] = field(default_factory=lambda: list(DEFAULT_NOTIFY))
    cameras: list[str] = field(default_factory=list)
    desktop: bool = True

    def camera_passes(self, camera: str) -> bool:
        return not self.cameras or camera in self.cameras

    def matches(self, camera: str, kinds: list[str]) -> bool:
        return self.camera_passes(camera) and any(k in self.notify for k in kinds)


@dataclass
class OpenEvent:
    camera: str
    event_type: str
    kinds: list[str]
    start: int
    seen: float


@dataclass
class Change:
    """One `event` line to emit, with the event as it stands after the change."""

    phase: str
    id: str
    event: OpenEvent
    end: int | None = None
    forced: bool = False  # ended by us (disconnect, reload, TTL sweep), not by Protect
    notify: bool = False  # run the event's actions: the filter newly matches it


class Events:
    """The console's open events, and the ids of recently ended ones: Protect keeps sending
    updates for an event after its end."""

    def __init__(self):
        self.open: dict[str, OpenEvent] = {}
        self.ended: OrderedDict[str, None] = OrderedDict()

    def apply(self, action: str, item: dict, filter: Filter, known, now: float) -> list[Change]:
        changes: list[Change] = []
        event_id = item.get("id")
        if not isinstance(event_id, str) or event_id in self.ended:
            return changes
        types = item.get("smartDetectTypes")
        end = item.get("end") if isinstance(item.get("end"), int) else None
        if event_id in self.open:
            event = self.open[event_id]
            if isinstance(types, list):
                kinds = event_log.event_kinds(event.event_type, types)
                if kinds != event.kinds:
                    old = event.kinds
                    event.kinds = kinds
                    notify = filter.camera_passes(event.camera) and any(
                        k in filter.notify and k not in old for k in kinds)
                    changes.append(Change("update", event_id, _copy(event), notify=notify))
            if end is not None:
                changes += self._end(event_id, end, False)
            return changes

        # Unseen id: an `add`, or a born-closed `update` carrying the complete event.
        event_type, start, camera = item.get("type"), item.get("start"), item.get("device")
        if not (isinstance(event_type, str) and isinstance(start, int) and isinstance(camera, str)):
            return changes
        if action not in ("add", "update") or (action == "update" and end is None):
            return changes
        if event_type not in event_log.EVENT_TYPES or not known(camera):
            return changes
        event = OpenEvent(camera, event_type, event_log.event_kinds(event_type, types), start, now)
        changes.append(Change("start", event_id, _copy(event),
                              notify=filter.matches(camera, event.kinds)))
        self.open[event_id] = event
        if end is not None:
            changes += self._end(event_id, end, False)
        return changes

    def _end(self, event_id: str, end: int, forced: bool) -> list[Change]:
        event = self.open.pop(event_id, None)
        if event is None:
            return []
        self.ended[event_id] = None
        while len(self.ended) > ENDED_REMEMBERED:
            self.ended.popitem(last=False)
        return [Change("end", event_id, event, end=end, forced=forced)]

    def end_all(self, end: int) -> list[Change]:
        return [c for event_id in list(self.open) for c in self._end(event_id, end, True)]

    def end_stale(self, now: float, end: int) -> list[Change]:
        stale = [i for i, e in self.open.items() if now - e.seen > EVENT_TTL]
        return [c for event_id in stale for c in self._end(event_id, end, True)]


def _copy(event: OpenEvent) -> OpenEvent:
    return OpenEvent(event.camera, event.event_type, list(event.kinds), event.start, event.seen)


# ------------------------------------------------------------------------------- daemon


@dataclass
class Cam:
    info: protect.Camera
    kinds: list[str]
    seq: int
    snapshot: str


@dataclass
class Connection:
    config: Config
    task: asyncio.Task
    network: asyncio.Event
    client: protect.Protect | None = None
    warmer: private_api.Warmer | None = None


class Daemon:
    def __init__(self, live_argv: list[str], out=None):
        self.live_argv = live_argv  # how to start `sauron live`: [python, bin/sauron, "live"]
        self.out = out or sys.stdout
        runtime = config_mod.runtime_dir()
        self.snap_dir = runtime / "snap"
        self.events_dir = runtime / "events"
        self.config_path = config_mod.config_path()
        self.config_mtime: float | None = None
        self.generation = 0
        self.connection: Connection | None = None
        self.status: tuple = ("connecting", None, None, None)
        self.cameras: dict[str, Cam] = {}
        self.emitted_cameras: list | None = None
        self.events = Events()
        self.filter = Filter()
        self.watch_task: asyncio.Task | None = None
        self.in_flight: set[str] = set()
        self.snapshot_errors: dict[str, str] = {}
        self.seq = 0
        self.event_log: Path | None = None
        self.writer = event_log.Writer(lambda message: self.log("warn", message))
        self.notifier = Notifier(live_argv)
        self.setup = onboarding.Onboarding(self.config_path, self._setup_saved)
        self.background: set[asyncio.Task] = set()
        self.loop: asyncio.AbstractEventLoop | None = None

    # ---- output -------------------------------------------------------------------------

    def emit(self, message: dict) -> None:
        try:
            self.out.write(json.dumps(message, ensure_ascii=False, separators=(",", ":")) + "\n")
            self.out.flush()
        except (BrokenPipeError, ValueError):
            os._exit(0)  # the shell is gone

    def log(self, level: str, message: str) -> None:
        self.emit({"t": "log", "level": level, "message": message})

    def set_status(self, state: str, message=None, protect_version=None, via=None) -> None:
        status = (state, message, protect_version, via)
        if status == self.status:
            return
        self.status = status
        self.emit({"t": "status", "state": state, "message": message, "protect": protect_version,
                   "via": via})

    @property
    def online(self) -> bool:
        return self.status[0] == "online"

    def spawn(self, coroutine) -> asyncio.Task:
        task = asyncio.get_running_loop().create_task(coroutine)
        self.background.add(task)
        task.add_done_callback(self.background.discard)
        return task

    # ---- lifecycle ----------------------------------------------------------------------

    async def start(self, watch_system: bool = True) -> None:
        """Says hello, connects, and starts the background work. `watch_system` also follows
        network changes and resume from suspend (off in tests)."""
        self.loop = asyncio.get_running_loop()
        runtime = self.snap_dir.parent
        runtime.mkdir(mode=0o700, parents=True, exist_ok=True)
        os.chmod(runtime, 0o700)
        self.snap_dir.mkdir(exist_ok=True)
        self.events_dir.mkdir(exist_ok=True)
        self.emit({"t": "hello", "version": VERSION, "protocol": PROTOCOL})
        await self.reload("startup")
        self.spawn(self._poll_config())
        self.spawn(self._sweep())
        if watch_system:
            self.spawn(self._watch_network())
            self.spawn(self._watch_resume())

    async def stop(self) -> None:
        if self.connection:
            self.connection.task.cancel()
            await asyncio.gather(self.connection.task, return_exceptions=True)
        for task in list(self.background):
            task.cancel()
        await asyncio.gather(*self.background, return_exceptions=True)

    async def run(self) -> None:
        for sig in (signal.SIGTERM, signal.SIGINT):
            asyncio.get_running_loop().add_signal_handler(sig, lambda: os._exit(0))
        await self.start()
        await self._read_stdin()

    async def _sweep(self) -> None:
        while True:
            await asyncio.sleep(SWEEP_EVERY)
            self.publish(self.events.end_stale(time.monotonic(), now_ms()))

    async def _poll_config(self) -> None:
        while True:
            await asyncio.sleep(CONFIG_POLL)
            if _mtime(self.config_path) != self.config_mtime:
                await self.reload("config")

    async def _read_stdin(self) -> None:
        reader = asyncio.StreamReader(limit=1 << 20)
        await self.loop.connect_read_pipe(lambda: asyncio.StreamReaderProtocol(reader), sys.stdin)
        while line := await reader.readline():
            await self.command(line)
        os._exit(0)  # the shell is gone

    async def reload(self, why: str) -> None:
        """Restarts the connection: re-reading the config, or the same one after a resume."""
        if why == "resume" and self.connection is None:
            return
        previous, self.connection = self.connection, None
        if previous:
            previous.task.cancel()
        self.generation += 1
        self.publish(self.events.end_all(now_ms()))
        if why == "config":
            private_api.forget_session()
        if previous and why == "resume":
            loaded: Config | Exception = previous.config
        else:
            self.config_mtime = _mtime(self.config_path)
            try:
                loaded = await config_mod.load(self.config_path)
            except (config_mod.NotSetUp, config_mod.Invalid) as err:
                loaded = err
        if isinstance(loaded, Exception):
            self.set_status("unconfigured", str(loaded))
            self.event_log = None
            self.cameras.clear()
            self.emit_cameras()
            return
        self.set_status("connecting")
        self.event_log = loaded.event_log
        network = asyncio.Event()
        task = asyncio.get_running_loop().create_task(self._run_session(self.generation, loaded, network))
        warmer = private_api.Warmer(loaded.username) if loaded.username else None
        self.connection = Connection(loaded, task, network, warmer=warmer)

    def _current(self, generation: int) -> bool:
        return generation == self.generation and self.connection is not None

    # ---- commands -----------------------------------------------------------------------

    async def command(self, line: bytes) -> None:
        if not line.strip():
            return
        try:
            command = json.loads(line)
            if not isinstance(command, dict) or not isinstance(command.get("cmd"), str):
                raise ValueError("not a command object")
        except ValueError as err:
            # Never echo the line: setup commands carry the API key and password.
            self.log("warn", f"ignoring an invalid command: {err}")
            return
        name = command["cmd"]
        if name == "filter":
            if isinstance(command.get("notify"), list):
                self.filter.notify = [k for k in command["notify"] if isinstance(k, str)]
            if isinstance(command.get("cameras"), list):
                self.filter.cameras = [c for c in command["cameras"] if isinstance(c, str)]
            if isinstance(command.get("desktop"), bool):
                self.filter.desktop = command["desktop"]
        elif name == "watch":
            self._set_watch(command.get("on") is True, command.get("interval"))
        elif name == "snapshot":
            camera = command.get("camera")
            if isinstance(camera, str) and camera in self.cameras:
                self.fetch_snapshot(camera)
            else:
                self.log("warn", f"snapshot: unknown camera {camera}")
        elif name == "live":
            await self.live(str(command.get("camera", "")))
        elif name == "reload":
            await self.reload("config")
        elif name.startswith("setup."):
            self.spawn(self._setup(name.removeprefix("setup."), command))
        else:
            self.log("warn", f"ignoring unknown command {name!r}")

    async def _setup(self, op: str, command: dict) -> None:
        reply = await self.setup.handle(op, command)
        self.emit({"t": "setup", "op": op, **reply})

    async def _setup_saved(self) -> None:
        await self.reload("config")

    def _set_watch(self, on: bool, interval) -> None:
        if self.watch_task:
            self.watch_task.cancel()
            self.watch_task = None
        if not on:
            return
        seconds = interval if isinstance(interval, (int, float)) and interval == interval else 2.0
        self.watch_task = self.spawn(self._watch_loop(min(max(float(seconds), 1.0), 60.0)))

    async def _watch_loop(self, seconds: float) -> None:
        while True:
            if self.online:
                for camera in [c.info.id for c in self.cameras.values() if c.info.online]:
                    self.fetch_snapshot(camera)
            await asyncio.sleep(seconds)

    # ---- cameras ------------------------------------------------------------------------

    def set_cameras(self, cameras: list[protect.Camera]) -> None:
        previous, self.cameras = self.cameras, {}
        for info in cameras:
            seq = previous[info.id].seq if info.id in previous else 0
            self.cameras[info.id] = Cam(info, info.kinds(), seq, str(self.snap_dir / f"{info.id}.jpg"))
        self.emit_cameras()

    def patch_camera(self, item: dict) -> None:
        ids = item.get("id")
        for camera_id in ids if isinstance(ids, list) else [ids]:
            cam = self.cameras.get(camera_id) if isinstance(camera_id, str) else None
            if cam is None:
                continue
            if isinstance(item.get("name"), str):
                cam.info.name = item["name"]
            if isinstance(item.get("state"), str):
                cam.info.state = item["state"]
        self.emit_cameras()

    def emit_cameras(self) -> None:
        cams = sorted(self.cameras.values(), key=lambda c: (c.info.display_name.lower(), c.info.id))
        views = [(c.info.id, c.info.display_name, c.info.model or "", c.info.online, c.kinds) for c in cams]
        if views == self.emitted_cameras:
            return
        self.emitted_cameras = views
        self.emit({"t": "cameras", "cameras": [
            {"id": c.info.id, "name": c.info.display_name, "model": c.info.model or "",
             "online": c.info.online, "kinds": c.kinds, "snapshot": c.snapshot, "seq": c.seq}
            for c in cams]})

    def camera_name(self, camera: str) -> str:
        cam = self.cameras.get(camera)
        return cam.info.display_name if cam else camera

    # ---- snapshots ----------------------------------------------------------------------

    def _client(self) -> protect.Protect | None:
        return self.connection.client if self.connection else None

    def fetch_snapshot(self, camera: str) -> None:
        """A tracked low-quality refresh, unless one is already in flight for the camera."""
        client = self._client()
        if client is None or camera in self.in_flight:
            return
        self.in_flight.add(camera)

        async def job():
            try:
                await self.store_snapshot(client, camera, None)
                self.snapshot_errors.pop(camera, None)
            except Exception as err:  # noqa: BLE001  reported, never fatal
                self.snapshot_failed(camera, err)
            finally:
                self.in_flight.discard(camera)

        self.spawn(job())

    def snapshot_failed(self, camera: str, err: BaseException) -> None:
        message = str(err) or type(err).__name__
        if self.snapshot_errors.get(camera) != message:
            self.snapshot_errors[camera] = message
            self.log("warn", f"snapshot {self.camera_name(camera)}: {message}")

    async def store_snapshot(self, client: protect.Protect, camera: str, event: str | None) -> Path | None:
        """Replaces `snap/<camera>.jpg` (and `events/<event>.jpg` if given) with a fresh
        low-quality snapshot and announces it. Returns the event image path."""
        if not _safe_id(camera) or (event is not None and not _safe_id(event)):
            raise ValueError("refusing unsafe id in file name")
        jpeg = await client.snapshot(camera, False)
        _write_atomic(self.snap_dir / f"{camera}.jpg", jpeg)
        image = None
        if event is not None:
            image = self.events_dir / f"{event}.jpg"
            _write_atomic(image, jpeg)
        self.seq += 1
        cam = self.cameras.get(camera)
        if cam is not None:
            cam.seq = self.seq
            self.emit({"t": "snapshot", "camera": camera, "seq": self.seq, "path": cam.snapshot})
        return image

    # ---- events -------------------------------------------------------------------------

    def on_event(self, action: str, item: dict) -> None:
        changes = self.events.apply(action, item, self.filter, lambda c: c in self.cameras,
                                    time.monotonic())
        self.publish(changes)

    def publish(self, changes: list[Change]) -> None:
        """An `event` line per change, starts and ends in the event log, and the actions of
        events the filter newly matches."""
        for change in changes:
            event = change.event
            matched = self.filter.matches(event.camera, event.kinds)
            self.emit({"t": "event", "phase": change.phase, "id": change.id, "camera": event.camera,
                       "type": event.event_type, "kinds": event.kinds, "match": matched,
                       "start": event.start, "end": change.end})
            if self.event_log and change.phase != "update":
                self.writer.write(self.event_log, event_log.camera_line(
                    change.phase, change.id, self.camera_name(event.camera), event.camera,
                    event.event_type, event.kinds, event.start, change.end, matched, change.forced))
            if change.notify:
                self.event_actions(change.id, event)

    def event_actions(self, event_id: str, event: OpenEvent) -> None:
        """Notification first, then the snapshot and the notification again with the image;
        detached so the `event` line is never delayed."""
        connection = self.connection
        if connection is None or connection.client is None:
            return
        self.spawn(self._event_job(connection, connection.client, event_id, _copy(event),
                                   self.camera_name(event.camera), self.filter.desktop))

    async def _event_job(self, connection: Connection, client, event_id: str, event: OpenEvent,
                         camera_name: str, desktop: bool) -> None:
        async def notify(image):
            try:
                await self.notifier.notify(event_id, event.camera, camera_name, event.kinds,
                                           event.start, str(image) if image else None)
            except RuntimeError as err:
                self.log("warn", f"notification: {err}")

        async def first():
            if desktop:
                # An update (kinds grew) keeps the image the earlier notification shows.
                previous = self.events_dir / f"{event_id}.jpg"
                await notify(previous if previous.exists() else None)

        async def snapshot():
            try:
                image = await self.store_snapshot(client, event.camera, event_id)
            except Exception as err:  # noqa: BLE001
                self.snapshot_failed(event.camera, err)
                return None
            try:
                _prune_event_images(self.events_dir)
            except OSError as err:
                self.log("error", f"pruning event images: {err}")
            return image

        _, image = await asyncio.gather(first(), snapshot())
        if not desktop:
            return
        if image:
            await notify(image)
        # A click on the notification opens live video: have a session ready for it.
        if connection.warmer:
            try:
                await connection.warmer.warm(client)
            except Exception as err:  # noqa: BLE001
                self.log("warn", f"instant live: {err}")

    # ---- live ---------------------------------------------------------------------------

    async def live(self, camera: str) -> None:
        if self.connection is None:
            self.emit({"t": "live", "camera": camera, "ok": False, "message": config_mod.NOT_SET_UP})
            return
        if camera not in self.cameras:
            self.emit({"t": "live", "camera": camera, "ok": False, "message": "unknown camera"})
            return
        try:
            viewer = await asyncio.create_subprocess_exec(
                *self.live_argv, camera, stdin=asyncio.subprocess.DEVNULL,
                stdout=asyncio.subprocess.DEVNULL, stderr=asyncio.subprocess.PIPE, process_group=0)
        except OSError as err:
            self.emit({"t": "live", "camera": camera, "ok": False,
                       "message": f"cannot start the live viewer: {err}"})
            return
        self.emit({"t": "live", "camera": camera, "ok": True, "message": None})
        name = self.camera_name(camera)

        async def wait():
            _, stderr = await viewer.communicate()
            if viewer.returncode:
                detail = stderr.decode(errors="replace").strip().splitlines()
                self.log("warn", f"live view of {name}: {detail[-1] if detail else viewer.returncode}")

        self.spawn(wait())

    # ---- the session --------------------------------------------------------------------

    async def _run_session(self, generation: int, config: Config, network: asyncio.Event) -> None:
        """Connects to the first address that answers, streams both websockets, and reconnects
        with backoff until cancelled. A network change cuts a backoff short."""
        backoff = BACKOFF_MIN
        logged = ""
        while True:
            try:
                client, version = await protect.connect(config)
            except protect.ConnectError as err:
                summary = err.summary()
                if summary != logged:
                    for attempt in err.attempts:
                        self.log("warn", f"{attempt.address}: {attempt.error}")
                    logged = summary
                state = {"auth": "auth", "untrusted": "unconfigured"}.get(err.failure, "offline")
                message = err.message
            else:
                backoff = BACKOFF_MIN
                failure: Exception | None = None
                try:
                    if await self._session_once(generation, config, client, version, network):
                        continue  # a network change: reconnect now, trying `host` first again
                except Exception as err:  # noqa: BLE001  any failure ends the session
                    failure = err
                finally:
                    if self._current(generation):
                        self.connection.client = None
                        self.publish(self.events.end_all(now_ms()))
                    await client.close()
                full = f"{client.address}: {failure}"
                if full != logged:
                    self.log("warn", full)
                    logged = full
                if isinstance(failure, protect.AuthError):
                    state, message = "auth", str(failure)
                else:
                    state, message = "offline", protect.failure_message(client.address, failure)
            if state == "auth":
                wait = AUTH_BACKOFF
            else:
                wait, backoff = backoff, min(backoff * 2, BACKOFF_MAX)
            if self._current(generation):
                self.set_status(state, message)
            network.clear()
            try:
                async with asyncio.timeout(wait):
                    await network.wait()
                backoff = BACKOFF_MIN
            except TimeoutError:
                pass

    async def _session_once(self, generation, config, client, version, network) -> bool:
        """One session on `client`'s address. True: a network change calls for reconnecting."""
        cameras = await client.cameras()
        events_ws = await client.subscribe("events")
        try:
            devices_ws = await client.subscribe("devices")
        except BaseException:
            await events_ws.close()
            raise
        sockets = {"events": events_ws, "devices": devices_ws}
        if not self._current(generation):
            raise asyncio.CancelledError()
        self.connection.client = client
        self.set_cameras(cameras)
        self.set_status("online", None, version, client.via)
        seen = {topic: time.monotonic() for topic in sockets}
        tasks = [
            asyncio.create_task(self._read_events(generation, events_ws, seen)),
            asyncio.create_task(self._read_devices(generation, client, devices_ws, seen)),
            asyncio.create_task(self._watchdog(config, client, sockets, seen, network)),
        ]
        try:
            done, _ = await asyncio.wait(tasks, return_when=asyncio.FIRST_COMPLETED)
            return next(iter(done)).result() is True
        finally:
            for task in tasks:
                task.cancel()
            await asyncio.gather(*tasks, return_exceptions=True)
            for socket in sockets.values():
                await socket.close()

    async def _read_events(self, generation, socket: net.WebSocket, seen: dict) -> None:
        while True:
            message = await socket.recv()
            seen["events"] = time.monotonic()
            frame = _frame(message)
            if frame is None:
                if isinstance(message, str):
                    self.log("warn", "unparsable event frame")
                continue
            if self._current(generation):
                self.on_event(frame[0], frame[1])

    async def _read_devices(self, generation, client, socket: net.WebSocket, seen: dict) -> None:
        while True:
            message = await socket.recv()
            seen["devices"] = time.monotonic()
            frame = _frame(message)
            if frame is None or frame[1].get("modelKey") != "camera" or not self._current(generation):
                continue
            action, item = frame
            if action == "update":
                self.patch_camera(item)
            elif action in ("add", "remove"):
                self.spawn(self._refetch_cameras(generation, client))

    async def _refetch_cameras(self, generation, client) -> None:
        try:
            cameras = await client.cameras()
        except Exception as err:  # noqa: BLE001
            self.log("warn", f"refreshing cameras: {err}")
            return
        if self._current(generation):
            self.set_cameras(cameras)

    async def _watchdog(self, config, client, sockets: dict, seen: dict, network: asyncio.Event) -> bool:
        """Pings both sockets, notices silence, and after a network change decides whether to
        reconnect: on a fallback, go home once `host` answers; else only if the address died."""
        last_ping = time.monotonic()
        network.clear()
        while True:
            try:
                async with asyncio.timeout(WATCHDOG_TICK):
                    await network.wait()
            except TimeoutError:
                now = time.monotonic()
                for topic, at in seen.items():
                    if now - at > DEAD_AFTER:
                        raise net.TransportError(f"{topic} websocket silent for {DEAD_AFTER:g}s")
                if now - last_ping >= PING_EVERY:
                    last_ping = now
                    for socket in sockets.values():
                        await socket.ping()
                continue
            network.clear()
            if await _needs_reconnect(config, client):
                return True

    # ---- system watchers ----------------------------------------------------------------

    def network_changed(self) -> None:
        if self.connection:
            self.connection.network.set()

    async def _watch_network(self) -> None:
        """`ip -o monitor route`: each burst of route changes, once settled, is a network change."""
        try:
            process = await asyncio.create_subprocess_exec(
                "ip", "-o", "monitor", "route", stdin=asyncio.subprocess.DEVNULL,
                stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.DEVNULL)
        except OSError as err:
            self.log("warn", f"cannot watch for network changes: {err}")
            return
        settle: asyncio.TimerHandle | None = None
        try:
            while await process.stdout.readline():
                if settle:
                    settle.cancel()
                settle = self.loop.call_later(NETWORK_SETTLE, self.network_changed)
        finally:
            if process.returncode is None:
                process.kill()
        self.log("warn", "cannot watch for network changes: `ip monitor route` exited")

    async def _watch_resume(self) -> None:
        """logind's PrepareForSleep(false) on the system bus: back from suspend."""
        try:
            process = await asyncio.create_subprocess_exec(
                "gdbus", "monitor", "--system", "--dest", "org.freedesktop.login1",
                "--object-path", "/org/freedesktop/login1", stdin=asyncio.subprocess.DEVNULL,
                stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.DEVNULL)
        except OSError as err:
            self.log("warn", f"cannot watch for resume from suspend: {err}")
            return
        try:
            while line := await process.stdout.readline():
                if b".PrepareForSleep (false" in line:
                    await self.reload("resume")
        finally:
            if process.returncode is None:
                process.kill()
        self.log("warn", "cannot watch for resume from suspend: `gdbus monitor` exited")


def _frame(message) -> tuple[str, dict] | None:
    """`(action, item)` of a subscription frame."""
    if not isinstance(message, str):
        return None
    try:
        frame = json.loads(message)
    except ValueError:
        return None
    if not isinstance(frame, dict) or not isinstance(frame.get("item"), dict):
        return None
    return str(frame.get("type", "")), frame["item"]


async def _needs_reconnect(config: Config, client: protect.Protect) -> bool:
    if client.via:
        home = protect.Protect(config, config.host)
        try:
            await home.alive()
            return True
        except Exception:  # noqa: BLE001  home is still away
            pass
        finally:
            await home.close()
    try:
        await client.alive()
        return False
    except Exception:  # noqa: BLE001
        return True


def _mtime(path: Path) -> float | None:
    try:
        return path.stat().st_mtime
    except OSError:
        return None


def _write_atomic(path: Path, data: bytes) -> None:
    tmp = path.with_name(path.name + ".tmp")
    tmp.write_bytes(data)
    os.replace(tmp, path)


def _prune_event_images(directory: Path) -> None:
    """Keeps only the newest event images."""
    images = []
    for entry in os.scandir(directory):
        if entry.name.endswith(".jpg"):
            images.append((entry.stat().st_mtime, entry.path))
    images.sort(reverse=True)
    for _, path in images[EVENT_IMAGES_KEPT:]:
        try:
            os.unlink(path)
        except FileNotFoundError:
            pass


async def main(live_argv: list[str]) -> None:
    await Daemon(live_argv).run()
