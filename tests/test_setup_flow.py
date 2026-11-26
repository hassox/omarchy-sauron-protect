"""End to end: the daemon, driven through the JSON lines the panel sends, sets itself up against
the fake console, goes online and reports a detection."""

import asyncio
import json
import os
import shutil
import tempfile
import tomllib
import unittest
from contextlib import ExitStack
from pathlib import Path
from unittest import mock

from tests import fakes, support
import mock_protect
import sauron
from sauron import daemon as daemon_mod

API_KEY = "key-Zq81-secret"
PASSWORD = "pw-Hx52-secret"


class SetupFlowTest(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.stack = ExitStack()
        self.env = fakes.Environment(self.stack)
        self.keyring = fakes.Keyring().patch(self.stack)
        self.notified = []

        async def notify(_self, event_id, camera_id, camera_name, kinds, start_ms, image=None):
            self.notified.append((event_id, camera_name, kinds, image))

        self.stack.enter_context(mock.patch("sauron.notify.Notifier.notify", notify))
        self.mock = await support.console(api_key=API_KEY, password=PASSWORD)
        self.host = f"127.0.0.1:{self.mock.port}"
        self.out = fakes.Output()
        self.daemon = daemon_mod.Daemon(["sauron-live"], out=self.out)
        await self.daemon.start(watch_system=False)

    async def asyncTearDown(self):
        await self.daemon.stop()
        await self.mock.stop()
        self.stack.close()

    async def send(self, command: dict) -> None:
        await self.daemon.command(json.dumps(command).encode() + b"\n")

    async def setup(self, op: str, **fields) -> dict:
        after = len(self.out.messages)
        await self.send({"cmd": f"setup.{op}", **fields})
        return await self.out.wait_for(lambda m: m.get("t") == "setup" and m.get("op") == op,
                                       timeout=10, after=after)

    async def probe(self) -> str:
        found = await self.setup("probe", host=self.host)
        self.assertTrue(found["ok"], found)
        return found["cert"]["sha256"]

    async def test_starts_unconfigured_and_asks_for_setup(self):
        self.assertEqual(self.out.messages[0], {"t": "hello", "version": sauron.VERSION, "protocol": 2})
        status = self.out.of("status")[-1]
        self.assertEqual(status["state"], "unconfigured")
        self.assertIn("panel", status["message"])
        loaded = await self.setup("load")
        self.assertEqual((loaded["configured"], loaded["hasKey"]), (False, False))

    async def test_a_garbled_command_is_not_echoed(self):
        await self.daemon.command(b'{"cmd": "setup.save", "apiKey": "' + API_KEY.encode() + b'"\n')
        warning = self.out.of("log")[-1]
        self.assertIn("invalid command", warning["message"])
        self.assertNotIn(API_KEY, self.out.text)

    async def test_probe_finds_the_console_and_shows_its_certificate(self):
        found = await self.setup("probe", host=self.host)
        self.assertTrue(found["console"])
        self.assertFalse(found["plain"])
        self.assertEqual(found["cert"]["sha256"].replace(":", "").lower(), support.pin().hex())
        if shutil.which("openssl"):
            self.assertEqual(found["cert"]["cn"], "unifi.mock")
            self.assertTrue(found["cert"]["selfSigned"])
        self.assertEqual(self.mock.seen and self.mock.seen[-1].headers.get("x-api-key"), None,
                         "probing sends no key")

    async def test_probe_reports_an_unreachable_address(self):
        found = await self.setup("probe", host="127.0.0.1:1")
        self.assertEqual((found["ok"], found["field"]), (False, "host"))
        self.assertIn("Cannot reach", found["message"])

    async def test_a_wrong_key_is_refused_and_nothing_is_saved(self):
        pin = await self.probe()
        reply = await self.setup("save", host=self.host, pin=pin, apiKey="wrong")
        self.assertEqual((reply["ok"], reply["field"]), (False, "key"))
        self.assertFalse(self.env.config.exists())

    async def test_another_machine_as_fallback_is_refused(self):
        other_dir = Path(self.stack.enter_context(tempfile.TemporaryDirectory()))
        other = await mock_protect.MockConsole(
            tls=mock_protect.server_context(*mock_protect.self_signed(other_dir))).start()
        self.addAsyncCleanup(other.stop)
        pin = await self.probe()
        reply = await self.setup("save", host=self.host, pin=pin, apiKey=API_KEY,
                                 fallbacks=[f"127.0.0.1:{other.port}"])
        self.assertEqual((reply["ok"], reply["field"]), (False, "fallbacks"))
        self.assertIn("different machine", reply["message"])
        self.assertEqual(other.seen, [], "nothing may reach the other machine")

    async def test_wrong_instant_live_password_is_refused(self):
        pin = await self.probe()
        reply = await self.setup("save", host=self.host, pin=pin, apiKey=API_KEY,
                                 username="sauron", password="nope")
        self.assertEqual((reply["ok"], reply["field"]), (False, "live"))
        self.assertIn("rejected", reply["message"])
        self.assertEqual(self.keyring.secrets, {})

    async def test_full_setup_goes_online_and_reports_detections(self):
        pin = await self.probe()
        reply = await self.setup("save", host=self.host, pin=pin, apiKey=API_KEY,
                                 fallbacks=["127.0.0.1:1"], username="sauron", password=PASSWORD)
        self.assertTrue(reply["ok"], reply)
        self.assertEqual(reply["message"], "Protect 6.1.0-mock · 5 cameras")

        # The config: private, pinned, with the key; the password went to the keyring only.
        self.assertEqual(self.env.config.stat().st_mode & 0o777, 0o600)
        saved = tomllib.loads(self.env.config.read_text())
        self.assertEqual(saved["host"], self.host)
        self.assertEqual(saved["cert_sha256"], pin)
        self.assertEqual(saved["api_key"], API_KEY)
        self.assertEqual(saved["fallback_hosts"], ["127.0.0.1:1"])
        self.assertEqual(saved["username"], "sauron")
        self.assertNotIn(PASSWORD, self.env.config.read_text())
        self.assertEqual(self.keyring.secrets, {(self.host, "sauron"): PASSWORD})

        online = await self.out.wait_for(lambda m: m.get("t") == "status" and m["state"] == "online")
        self.assertEqual(online["protect"], "6.1.0-mock")
        cameras = self.out.of("cameras")[-1]["cameras"]
        self.assertEqual(len(cameras), 5)
        front = next(c for c in cameras if c["name"] == "Front Door")
        self.assertIn("ring", front["kinds"])

        # A detection: start and end lines, a notification and an event image.
        after = len(self.out.messages)
        event_id = await self.mock.trigger("Front Door", "person", 0.05)
        start = await self.out.wait_for(lambda m: m.get("t") == "event" and m["phase"] == "start", after=after)
        self.assertEqual((start["id"], start["camera"], start["kinds"], start["match"]),
                         (event_id, "cam-front", ["person"], True))
        await self.out.wait_for(lambda m: m.get("t") == "event" and m["phase"] == "end", after=after)
        await self.out.wait_for(lambda m: m.get("t") == "snapshot" and m["camera"] == "cam-front", after=after)
        for _ in range(50):
            if len(self.notified) >= 2:
                break
            await asyncio.sleep(0.02)
        self.assertEqual(self.notified[0][:3], (event_id, "Front Door", ["person"]))
        self.assertEqual(self.notified[-1][3], str(self.env.runtime / "events" / f"{event_id}.jpg"))

        # Kinds the bar doesn't alert on are reported, but not matched or notified.
        notified = len(self.notified)
        after = len(self.out.messages)
        await self.mock.trigger("Backyard", "animal", 0.01)
        animal = await self.out.wait_for(lambda m: m.get("t") == "event" and m["phase"] == "start", after=after)
        self.assertFalse(animal["match"])
        self.assertEqual(len(self.notified), notified)

        # Secrets never appear in anything the daemon prints.
        self.assertNotIn(API_KEY, self.out.text)
        self.assertNotIn(PASSWORD, self.out.text)

    async def test_snapshots_refresh_while_the_panel_watches(self):
        pin = await self.probe()
        self.assertTrue((await self.setup("save", host=self.host, pin=pin, apiKey=API_KEY))["ok"])
        await self.out.wait_for(lambda m: m.get("t") == "status" and m["state"] == "online")
        after = len(self.out.messages)
        await self.send({"cmd": "watch", "on": True, "interval": 1})
        seen = set()
        while len(seen) < 5:
            message = await self.out.wait_for(
                lambda m: m.get("t") == "snapshot" and m["camera"] not in seen, after=after)
            seen.add(message["camera"])
            self.assertTrue(os.path.exists(message["path"]))
        await self.send({"cmd": "watch", "on": False})

    async def test_resaving_keeps_the_key_and_password(self):
        pin = await self.probe()
        await self.setup("save", host=self.host, pin=pin, apiKey=API_KEY, username="sauron", password=PASSWORD)
        loaded = await self.setup("load")
        self.assertEqual((loaded["configured"], loaded["hasKey"], loaded["hasPassword"]), (True, True, True))
        self.assertEqual(loaded["pin"], pin)
        self.assertNotIn(API_KEY, json.dumps(loaded))
        # Blank key and password fields keep what is saved.
        again = await self.setup("save", host=self.host, pin=pin, apiKey="", username="sauron", password="")
        self.assertTrue(again["ok"], again)
        self.assertEqual(tomllib.loads(self.env.config.read_text())["api_key"], API_KEY)

    async def test_event_log_records_starts_and_ends(self):
        pin = await self.probe()
        await self.setup("save", host=self.host, pin=pin, apiKey=API_KEY)
        log = self.env.dir / "events.jsonl"
        with self.env.config.open("a") as file:
            file.write("")
        text = self.env.config.read_text().replace('event_log = ""', f'event_log = "{log}"')
        self.env.config.write_text(text)
        await self.daemon.reload("config")
        await self.out.wait_for(lambda m: m.get("t") == "status" and m["state"] == "online",
                                after=len(self.out.messages) - 1)
        event_id = await self.mock.trigger("Driveway", "vehicle", 0.01)
        for _ in range(100):
            if log.exists() and len(log.read_text().splitlines()) >= 2:
                break
            await asyncio.sleep(0.02)
        lines = [json.loads(line) for line in log.read_text().splitlines()]
        self.assertEqual([(l["phase"], l["id"], l["camera"]) for l in lines],
                         [("start", event_id, "Driveway"), ("end", event_id, "Driveway")])
        self.assertTrue(lines[1]["match"])


if __name__ == "__main__":
    unittest.main()
