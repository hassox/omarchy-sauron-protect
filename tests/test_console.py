"""The Protect and private-API clients against the fake console, over real (pinned) TLS."""

import json
import unittest
from contextlib import ExitStack

from tests import fakes, support
from sauron import net, private_api, protect
from sauron.config import Config


def config(mock, **changes) -> Config:
    values = dict(host=f"127.0.0.1:{mock.port}", fallback_hosts=(), pin=support.pin(), api_key="mock")
    values.update(changes)
    return Config(**values)


class ProtectTest(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.mock = await support.console()

    async def asyncTearDown(self):
        await self.mock.stop()

    async def test_connect_lists_cameras_and_versions(self):
        client, version = await protect.connect(config(self.mock))
        self.assertEqual(version, "6.1.0-mock")
        self.assertIsNone(client.via)
        cameras = await client.cameras()
        self.assertEqual([c.display_name for c in protect.sorted_cameras(cameras)][:2], ["Backyard", "Driveway"])
        self.assertEqual(await client.snapshot("cam-drive"), self.mock._picture(("cam-drive", "Driveway")))
        await client.close()

    async def test_falls_back_to_the_next_address_and_reports_it(self):
        dead = "127.0.0.1:1"
        client, _ = await protect.connect(config(self.mock, host=dead, fallback_hosts=(f"127.0.0.1:{self.mock.port}",)))
        self.assertEqual(client.via, f"127.0.0.1:{self.mock.port}")
        # The console's identity stays the configured host, whichever address answered.
        self.assertEqual(client.console_id, "127.0.0.1:1")
        await client.close()

    async def test_a_rejected_key_stops_the_search(self):
        with self.assertRaises(protect.ConnectError) as caught:
            await protect.connect(config(self.mock, api_key="wrong", fallback_hosts=("127.0.0.1:1",)))
        self.assertEqual(caught.exception.failure, "auth")
        self.assertEqual(len(caught.exception.attempts), 1)

    async def test_unpinned_self_signed_console_needs_setup(self):
        with self.assertRaises(protect.ConnectError) as caught:
            await protect.connect(config(self.mock, pin=None))
        self.assertEqual(caught.exception.failure, "untrusted")
        self.assertEqual(self.mock.seen, [])

    async def test_stream_url_is_created_and_pointed_at_the_console(self):
        client, _ = await protect.connect(config(self.mock, live_quality="medium"))
        url = await client.stream_url("cam-back", "medium")
        self.assertEqual(protect.fixup_stream_url(url, client.hostname),
                         "rtsps://127.0.0.1:7441/cam-back-medium")
        await client.close()


class PrivateApiTest(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.stack = ExitStack()
        self.env = fakes.Environment(self.stack)
        self.mock = await support.console()
        self.client = protect.Protect(config(self.mock), f"127.0.0.1:{self.mock.port}")

    async def asyncTearDown(self):
        await self.client.close()
        await self.mock.stop()
        self.stack.close()

    def api(self, username="sauron", password="mock"):
        return private_api.PrivateApi(self.client, username, password)

    async def test_login_caches_the_session_privately(self):
        await self.api().login()
        path = private_api.session_path()
        self.assertEqual(path.stat().st_mode & 0o777, 0o600)
        session = json.loads(path.read_text())
        self.assertTrue(session["cookie"].startswith("TOKEN="))
        self.assertTrue(session["csrf"].startswith("csrf-"))
        # Another process reuses it without logging in again.
        logins = sum(1 for s in self.mock.seen if s.path == "/api/auth/login")
        await self.api().events(0, 2**53, 10)
        self.assertEqual(sum(1 for s in self.mock.seen if s.path == "/api/auth/login"), logins)

    async def test_bad_credentials_pause_automatic_logins(self):
        with self.assertRaises(private_api.LoginError) as caught:
            await self.api(password="wrong").login()
        self.assertEqual(caught.exception.kind, "credentials")
        attempts = len(self.mock.seen)
        with self.assertRaises(private_api.LoginError) as paused:
            await self.api(password="wrong").events(0, 1, 1)
        self.assertEqual(paused.exception.kind, "paused")
        self.assertEqual(len(self.mock.seen), attempts, "a paused login must not reach the console")
        # A login the user asks for ignores the pause, and clears it on success.
        await self.api().login()
        self.assertFalse(private_api.pause_path().exists())

    async def test_mfa_accounts_are_refused_with_a_reason(self):
        with self.assertRaises(private_api.LoginError) as caught:
            await self.api(username="mfa").login()
        self.assertEqual(caught.exception.kind, "mfa")
        self.assertIn("MFA", str(caught.exception))

    async def test_too_many_failures_are_rate_limited(self):
        for _ in range(5):
            with self.assertRaises(private_api.LoginError):
                await self.api(password="wrong").login()
        with self.assertRaises(private_api.LoginError) as caught:
            await self.api(password="wrong").login()
        self.assertEqual(caught.exception.kind, "rate")
        self.assertEqual(caught.exception.wait, 60)

    async def test_an_expired_session_logs_in_again_once(self):
        api = self.api()
        await api.login()
        self.mock.sessions.clear()
        body = await api.events(0, 2**53, 5)
        self.assertEqual(len(json.loads(body)), 5)

    async def test_livestream_url_points_at_the_active_address(self):
        url = await self.api().livestream_url("cam-front", 1)
        self.assertTrue(url.startswith(f"wss://127.0.0.1:{self.mock.port}/ws/livestream?"), url)
        self.assertIn("camera=cam-front", url)

    async def test_thumbnails_and_missing_ones(self):
        api = self.api()
        self.assertEqual(await api.event_thumbnail("hist-1"), self.mock._picture(("cam-drive", "Driveway")))
        self.assertIsNone(await api.event_thumbnail("hist-admin"))
        self.assertIsNone(await api.event_thumbnail("nope"))

    async def test_livestream_websocket_carries_fmp4_frames(self):
        from sauron.live import Demux
        url = await self.api().livestream_url("cam-front", 0)
        socket = await net.WebSocket.connect(url, support.pin(), max_size=None)
        demux, out = Demux(), b""
        while b"moof2" not in out:
            out += demux.push(await socket.recv())
        await socket.close()
        self.assertTrue(out.startswith(b"ftyp-moov-init"))
        self.assertIn(b"moof1mdat1", out)


if __name__ == "__main__":
    unittest.main()
