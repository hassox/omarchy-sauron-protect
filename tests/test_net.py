import asyncio
import json
import unittest

from tests import support
from sauron import net

API = "/proxy/protect/integration"


class RecordingServer:
    """A TLS server that records every byte a client sends after the handshake."""

    def __init__(self):
        self.received = bytearray()
        self.connections = 0

    async def start(self):
        self.server = await asyncio.start_server(self._serve, "127.0.0.1", 0, ssl=support.server_tls())
        self.port = self.server.sockets[0].getsockname()[1]
        return self

    async def _serve(self, reader, writer):
        self.connections += 1
        try:
            while data := await reader.read(65536):
                self.received += data
        except (ConnectionError, OSError):
            pass
        writer.close()

    async def stop(self):
        self.server.close()
        await self.server.wait_closed()


class FingerprintTest(unittest.TestCase):
    def test_parse_accepts_any_case_with_or_without_colons(self):
        digest = bytes(range(32))
        text = net.fingerprint_text(digest)
        self.assertEqual(net.parse_fingerprint(text), digest)
        self.assertEqual(net.parse_fingerprint(text.lower().replace(":", "")), digest)
        self.assertIsNone(net.parse_fingerprint(text[:-3]))
        self.assertIsNone(net.parse_fingerprint("zz" * 32))

    def test_short_form_shows_both_ends(self):
        self.assertEqual(net.fingerprint_short(bytes(range(32))), "00:01:02:…:1C:1D:1E:1F")


class HttpTest(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.mock = await support.console()
        self.endpoint = net.Endpoint("https", "127.0.0.1", self.mock.port)

    async def asyncTearDown(self):
        await self.mock.stop()

    async def test_pinned_request_reaches_the_console(self):
        client = net.HttpClient(self.endpoint, support.pin())
        response = await client.request("GET", API + "/v1/meta/info", [("X-API-KEY", "mock")])
        self.assertEqual(response.status, 200)
        self.assertEqual(json.loads(response.body)["applicationVersion"], "6.1.0-mock")
        await client.close()

    async def test_connections_are_kept_alive(self):
        client = net.HttpClient(self.endpoint, support.pin())
        for _ in range(3):
            await client.request("GET", API + "/v1/meta/info", [("X-API-KEY", "mock")])
        self.assertEqual(len(client._idle), 1)
        await client.close()

    async def test_unpinned_self_signed_console_is_rejected_with_its_leaf(self):
        client = net.HttpClient(self.endpoint, None)
        with self.assertRaises(net.Rejection) as caught:
            await client.request("GET", API + "/v1/meta/info", [("X-API-KEY", "mock")])
        self.assertEqual(caught.exception.kind, "untrusted")
        self.assertEqual(caught.exception.leaf, support.leaf_der())
        self.assertEqual(self.mock.seen, [], "nothing may reach an untrusted console")

    async def test_redirects_are_returned_not_followed(self):
        self.mock.redirect_to = "https://elsewhere.example"
        client = net.HttpClient(self.endpoint, support.pin())
        response = await client.request("GET", API + "/v1/meta/info", [("X-API-KEY", "mock")])
        self.assertEqual(response.status, 302)
        self.assertEqual(len(self.mock.seen), 1)
        await client.close()

    async def test_closed_port_is_a_transport_error(self):
        await self.mock.stop()
        client = net.HttpClient(self.endpoint, support.pin(), connect_timeout=1)
        with self.assertRaises(net.TransportError) as caught:
            await client.request("GET", "/")
        self.assertNotIsInstance(caught.exception, net.Rejection)


class PinMismatchTest(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.server = await RecordingServer().start()
        self.endpoint = net.Endpoint("https", "127.0.0.1", self.server.port)

    async def asyncTearDown(self):
        await self.server.stop()

    async def test_http_sends_nothing_to_a_mismatched_certificate(self):
        client = net.HttpClient(self.endpoint, bytes(32))
        with self.assertRaises(net.Rejection) as caught:
            await client.request("GET", "/", [("X-API-KEY", "secret-key")], body=b"password")
        self.assertEqual(caught.exception.kind, "mismatch")
        self.assertEqual(caught.exception.leaf, support.leaf_der())
        await asyncio.sleep(0.05)
        self.assertEqual(self.server.connections, 1)
        self.assertEqual(bytes(self.server.received), b"")

    async def test_websocket_sends_nothing_to_a_mismatched_certificate(self):
        url = f"wss://127.0.0.1:{self.server.port}/v1/subscribe/events"
        with self.assertRaises(net.Rejection):
            await net.WebSocket.connect(url, bytes(32), [("X-API-KEY", "secret-key")])
        await asyncio.sleep(0.05)
        self.assertEqual(bytes(self.server.received), b"")


class WebSocketTest(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.mock = await support.console()
        self.url = f"wss://127.0.0.1:{self.mock.port}{API}/v1/subscribe/events"

    async def asyncTearDown(self):
        await self.mock.stop()

    async def test_receives_text_frames_and_answers_pings(self):
        socket = await net.WebSocket.connect(self.url, support.pin(), [("X-API-KEY", "mock")])
        await socket.ping()
        trigger = asyncio.create_task(self.mock.trigger("Driveway", "person", 0.01))
        add = json.loads(await asyncio.wait_for(socket.recv(), 2))
        end = json.loads(await asyncio.wait_for(socket.recv(), 2))
        await trigger
        self.assertEqual(add["type"], "add")
        self.assertEqual(add["item"]["smartDetectTypes"], ["person"])
        self.assertEqual(end["item"]["id"], add["item"]["id"])
        self.assertIn("end", end["item"])
        await socket.close()

    async def test_wrong_key_is_an_http_401(self):
        with self.assertRaises(net.WsHttpError) as caught:
            await net.WebSocket.connect(self.url, support.pin(), [("X-API-KEY", "nope")])
        self.assertEqual(caught.exception.status, 401)

    async def test_server_going_away_ends_recv(self):
        socket = await net.WebSocket.connect(self.url, support.pin(), [("X-API-KEY", "mock")])
        await self.mock.stop()
        with self.assertRaises(net.TransportError):
            await asyncio.wait_for(socket.recv(), 2)


if __name__ == "__main__":
    unittest.main()
