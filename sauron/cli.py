"""The `sauron` command."""

from __future__ import annotations

import argparse
import asyncio
import re
import sys
from pathlib import Path

from . import VERSION, config as config_mod, daemon, history, live, protect

BIN = Path(__file__).resolve().parent.parent / "bin" / "sauron"

DESCRIPTION = "UniFi Protect bridge for the Omarchy bar. Set it up from the Sauron panel."


def live_argv() -> list[str]:
    return [sys.executable, str(BIN), "live"]


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="sauron", description=DESCRIPTION)
    parser.add_argument("--version", action="version", version=f"sauron {VERSION}")
    commands = parser.add_subparsers(dest="command", metavar="command")
    commands.add_parser("watch", help="run the daemon (JSON lines on stdin/stdout)")
    commands.add_parser("check", help="verify the config and list cameras")
    commands.add_parser("cameras", help="list cameras")
    one = commands.add_parser("live", help="open the live view (camera id or name)")
    one.add_argument("camera")
    shot = commands.add_parser("snapshot", help="save a snapshot (default ./<name>.jpg)")
    shot.add_argument("camera")
    shot.add_argument("out", nargs="?")
    log = commands.add_parser(
        "log", help="print Protect's event history as JSON lines, oldest first",
        description="<when> is 30m, 24h, 7d, YYYY-MM-DD, \"YYYY-MM-DD HH:MM\" or RFC 3339.")
    log.add_argument("--since", default="24h", metavar="when")
    log.add_argument("--until", metavar="when")
    log.add_argument("--all", action="store_true",
                     help="add every other event type, including admin logins with IP addresses")
    thumb = commands.add_parser("thumbnail", help="save an event's thumbnail (default ./<id>.jpg)")
    thumb.add_argument("event")
    thumb.add_argument("out", nargs="?")
    return parser


async def _connected():
    config = await config_mod.load(config_mod.config_path())
    client, version = await protect.connect(config)
    return config, client, version


async def _check() -> None:
    config = await config_mod.load(config_mod.config_path())
    try:
        client, version = await protect.connect(config)
    except protect.ConnectError as err:
        for attempt in err.attempts:
            detail = f" ({attempt.error})" if attempt.outcome in ("no answer",) else ""
            print(f"✗ {attempt.address}: {attempt.outcome}{detail}", file=sys.stderr)
        raise RuntimeError("cannot connect to the console" if err.failure == "offline" else err.message) from None
    via = " via fallback" if client.via else ""
    print(f"Protect {version} at {client.address} ({client.security}){via}")
    await _list(client)


async def _list(client: protect.Protect) -> None:
    for camera in protect.sorted_cameras(await client.cameras()):
        state = "online" if camera.online else "offline"
        print(f"{state}\t{camera.display_name}\t{camera.model or ''}\t{camera.id}")


async def _snapshot(query: str, out: str | None) -> None:
    _, client, _ = await _connected()
    camera = protect.resolve(protect.sorted_cameras(await client.cameras()), query)
    safe_name = re.sub(r"[^\w-]", "_", camera.display_name)
    path = Path(out) if out else Path(f"./{safe_name}.jpg")
    jpeg = await client.snapshot(camera.id, camera.full_hd_snapshot)
    path.write_bytes(jpeg)
    print(path)


async def _run(args) -> None:
    if args.command == "watch":
        await daemon.main(live_argv())
    elif args.command == "check":
        await _check()
    elif args.command == "cameras":
        await _list((await _connected())[1])
    elif args.command == "live":
        config, client, _ = await _connected()
        await live.run(config, client, args.camera)
    elif args.command == "snapshot":
        await _snapshot(args.camera, args.out)
    elif args.command == "log":
        config, client, _ = await _connected()
        await history.log(config, client, args.since, args.until, args.all)
    elif args.command == "thumbnail":
        config, client, _ = await _connected()
        await history.thumbnail(config, client, args.event, args.out)


def main(argv: list[str] | None = None) -> int:
    parser = _parser()
    args = parser.parse_args(argv)
    if args.command is None:
        parser.print_help()
        return 0
    try:
        asyncio.run(_run(args))
    except KeyboardInterrupt:
        return 130
    except Exception as err:  # noqa: BLE001  one line for the user, not a traceback
        print(f"sauron: {err}", file=sys.stderr)
        return 1
    return 0
