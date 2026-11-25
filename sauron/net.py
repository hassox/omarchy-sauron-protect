"""Connections to the console.

Every connection is opened by `open_stream`, which checks the console's certificate before it
returns, so no byte of HTTP (and so no API key, password or cookie) is ever written to a peer that
failed the check. HTTP/1.1 (h11) and websockets (websockets' sans-I/O protocol) run on top of the
verified stream; neither library does any I/O of its own.
"""

from __future__ import annotations

import asyncio
import hashlib
import ssl
import time
from dataclasses import dataclass
from urllib.parse import urlsplit

import h11
from websockets.client import ClientProtocol
from websockets.exceptions import InvalidStatus
from websockets.frames import Frame, Opcode
from websockets.http11 import Response as WsResponse
from websockets.uri import parse_uri

READ_SIZE = 64 * 1024
# Snapshots and event lists are far below this; anything bigger is not the console.
MAX_BODY = 32 * 1024 * 1024
POOL_IDLE = 60.0
POOL_SIZE = 4


class TransportError(Exception):
    """A network or TLS failure: refused, timed out, reset, handshake failed."""


class Rejection(TransportError):
    """The console's certificate was refused. `leaf` is the DER it presented."""

    def __init__(self, kind: str, leaf: bytes, reason: str = ""):
        self.kind = kind  # "mismatch" (pin set, other leaf) or "untrusted" (no pin, not trusted)
        self.leaf = leaf
        self.reason = reason
        if kind == "mismatch":
            message = f"certificate mismatch (got SHA256 {fingerprint_text(sha256(leaf))})"
        else:
            message = f"untrusted certificate ({reason})" if reason else "untrusted certificate"
        super().__init__(message)


def sha256(der: bytes) -> bytes:
    return hashlib.sha256(der).digest()


def fingerprint_text(digest: bytes) -> str:
    """`AB:CD:…`, as openssl prints it."""
    return ":".join(f"{b:02X}" for b in digest)


def fingerprint_short(digest: bytes) -> str:
    """`AB:CD:EF:…:12:34:56:78`: enough to recognise at a glance."""
    return f"{fingerprint_text(digest[:3])}:…:{fingerprint_text(digest[28:])}"


def parse_fingerprint(text: str) -> bytes | None:
    """A SHA-256 fingerprint in any case, with or without colons."""
    digits = text.strip().replace(":", "")
    if len(digits) != 64:
        return None
    try:
        return bytes.fromhex(digits)
    except ValueError:
        return None


@dataclass(frozen=True)
class Endpoint:
    """Where to connect: `https` or `http`, host and port."""

    scheme: str
    host: str
    port: int

    @classmethod
    def of(cls, url: str) -> Endpoint:
        parts = urlsplit(url)
        scheme = {"wss": "https", "ws": "http"}.get(parts.scheme, parts.scheme)
        if scheme not in ("https", "http") or not parts.hostname:
            raise ValueError(f"unsupported URL {url!r}")
        port = parts.port or (443 if scheme == "https" else 80)
        return cls(scheme, parts.hostname, port)

    @property
    def authority(self) -> str:
        """`host[:port]` as it goes in a Host header."""
        host = f"[{self.host}]" if ":" in self.host else self.host
        default = 443 if self.scheme == "https" else 80
        return host if self.port == default else f"{host}:{self.port}"


def _context(verify: bool) -> ssl.SSLContext:
    if verify:
        context = ssl.create_default_context()
    else:
        # Pinned: any chain, name and dates, as long as the leaf is the pinned one. The handshake
        # still proves the peer holds that leaf's private key.
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
        context.check_hostname = False
        context.verify_mode = ssl.CERT_NONE
    context.minimum_version = ssl.TLSVersion.TLSv1_2
    context.set_alpn_protocols(["http/1.1"])
    return context


_PINNED = _context(False)
_TRUSTED: ssl.SSLContext | None = None


def _trusted() -> ssl.SSLContext:
    global _TRUSTED
    if _TRUSTED is None:
        _TRUSTED = _context(True)
    return _TRUSTED


async def _connect(endpoint: Endpoint, context: ssl.SSLContext | None, timeout: float):
    try:
        async with asyncio.timeout(timeout):
            return await asyncio.open_connection(
                endpoint.host,
                endpoint.port,
                ssl=context,
                server_hostname=endpoint.host if context else None,
                limit=READ_SIZE,
            )
    except TimeoutError as err:
        raise TransportError(f"no answer within {timeout:g}s") from err
    except ssl.SSLCertVerificationError:
        raise
    except (OSError, ssl.SSLError) as err:
        raise TransportError(_describe_os_error(err)) from err


def _describe_os_error(err: BaseException) -> str:
    if isinstance(err, ssl.SSLError):
        return f"TLS handshake failed ({err.reason or err})"
    if isinstance(err, OSError) and err.strerror:
        return err.strerror
    return str(err) or type(err).__name__


async def _leaf_of(endpoint: Endpoint, timeout: float) -> bytes:
    """The certificate the peer presents, read without verifying it and without sending HTTP."""
    _, writer = await _connect(endpoint, _PINNED, timeout)
    try:
        return writer.get_extra_info("ssl_object").getpeercert(binary_form=True) or b""
    finally:
        await _close(writer)


async def _close(writer: asyncio.StreamWriter) -> None:
    writer.close()
    try:
        async with asyncio.timeout(1):
            await writer.wait_closed()
    except (TimeoutError, OSError, ssl.SSLError):
        pass


async def open_stream(endpoint: Endpoint, pin: bytes | None, timeout: float):
    """A connection to `endpoint` whose certificate is the pinned one, or is publicly trusted
    when there is no pin. Raises `Rejection` (carrying the leaf) before anything is written."""
    if endpoint.scheme == "http":
        return await _connect(endpoint, None, timeout)
    if pin is not None:
        reader, writer = await _connect(endpoint, _PINNED, timeout)
        leaf = writer.get_extra_info("ssl_object").getpeercert(binary_form=True) or b""
        if not leaf or sha256(leaf) != pin:
            await _close(writer)
            raise Rejection("mismatch", leaf)
        return reader, writer
    try:
        return await _connect(endpoint, _trusted(), timeout)
    except ssl.SSLCertVerificationError as err:
        reason = err.verify_message or str(err)
    leaf = await _leaf_of(endpoint, timeout)
    raise Rejection("untrusted", leaf, reason)


# ---------------------------------------------------------------------------------------- HTTP


@dataclass
class Response:
    status: int
    headers: list[tuple[bytes, bytes]]
    body: bytes

    def header(self, name: str) -> str | None:
        key = name.lower().encode()
        for header, value in self.headers:
            if header == key:
                return value.decode("latin-1")
        return None

    def header_all(self, name: str) -> list[str]:
        key = name.lower().encode()
        return [value.decode("latin-1") for header, value in self.headers if header == key]


class _Conn:
    def __init__(self, reader, writer):
        self.reader = reader
        self.writer = writer
        self.h11 = h11.Connection(h11.CLIENT, max_incomplete_event_size=64 * 1024)
        self.idle_since = time.monotonic()


class HttpClient:
    """HTTP/1.1 to one endpoint, with a few kept-alive connections. Redirects are returned, never
    followed: they would carry the API key or a login body past the checked console."""

    def __init__(self, endpoint: Endpoint, pin: bytes | None, connect_timeout: float = 3.0):
        self.endpoint = endpoint
        self.pin = pin
        self.connect_timeout = connect_timeout
        self._idle: list[_Conn] = []
        self._permits = asyncio.Semaphore(POOL_SIZE)

    async def request(
        self,
        method: str,
        target: str,
        headers: list[tuple[str, str]] | None = None,
        body: bytes | None = None,
        timeout: float = 20.0,
    ) -> Response:
        async with self._permits:
            try:
                async with asyncio.timeout(timeout):
                    return await self._request(method, target, headers or [], body)
            except TimeoutError as err:
                raise TransportError(f"no answer within {timeout:g}s") from err

    async def _request(self, method, target, headers, body) -> Response:
        conn, reused = self._take()
        if conn is None:
            reader, writer = await open_stream(self.endpoint, self.pin, self.connect_timeout)
            conn = _Conn(reader, writer)
        try:
            response = await self._exchange(conn, method, target, headers, body)
        except TransportError as err:
            await _close(conn.writer)
            # A kept-alive connection the console already dropped: retry once on a fresh one.
            if reused and method == "GET" and not isinstance(err, Rejection):
                return await self._request(method, target, headers, body)
            raise
        except BaseException:
            await _close(conn.writer)
            raise
        self._give_back(conn)
        return response

    def _take(self) -> tuple[_Conn | None, bool]:
        now = time.monotonic()
        while self._idle:
            conn = self._idle.pop()
            if now - conn.idle_since < POOL_IDLE and not conn.reader.at_eof():
                return conn, True
            conn.writer.close()
        return None, False

    def _give_back(self, conn: _Conn) -> None:
        if conn.h11.our_state is h11.DONE and conn.h11.their_state is h11.DONE:
            try:
                conn.h11.start_next_cycle()
            except h11.LocalProtocolError:
                conn.writer.close()
                return
            conn.idle_since = time.monotonic()
            self._idle.append(conn)
            while len(self._idle) > POOL_SIZE:
                self._idle.pop(0).writer.close()
        else:
            conn.writer.close()

    async def _exchange(self, conn: _Conn, method, target, headers, body) -> Response:
        all_headers = [("Host", self.endpoint.authority), ("User-Agent", "sauron")]
        all_headers += headers
        if body is not None:
            all_headers.append(("Content-Length", str(len(body))))
        h = conn.h11
        try:
            data = h.send(h11.Request(method=method, target=target, headers=all_headers))
            if body:
                data += h.send(h11.Data(data=body))
            data += h.send(h11.EndOfMessage())
            conn.writer.write(data)
            await conn.writer.drain()

            response = None
            chunks: list[bytes] = []
            size = 0
            while True:
                event = h.next_event()
                if event is h11.NEED_DATA:
                    data = await conn.reader.read(READ_SIZE)
                    h.receive_data(data)
                    continue
                if isinstance(event, h11.Response):
                    response = event
                elif isinstance(event, h11.Data):
                    size += len(event.data)
                    if size > MAX_BODY:
                        raise TransportError("response too large")
                    chunks.append(bytes(event.data))
                elif isinstance(event, h11.EndOfMessage):
                    break
                elif isinstance(event, h11.ConnectionClosed):
                    raise TransportError("connection closed by the console")
        except (OSError, ssl.SSLError) as err:
            raise TransportError(_describe_os_error(err)) from err
        except h11.ProtocolError as err:
            raise TransportError(f"bad HTTP from the console ({err})") from err
        assert response is not None
        return Response(response.status_code, list(response.headers), b"".join(chunks))

    async def close(self) -> None:
        idle, self._idle = self._idle, []
        for conn in idle:
            conn.writer.close()


# ----------------------------------------------------------------------------------- websockets


class WsHttpError(TransportError):
    """The console answered the upgrade with an HTTP status instead of switching protocols."""

    def __init__(self, status: int):
        self.status = status
        super().__init__(f"websocket refused: HTTP {status}")


class WsClosed(TransportError):
    pass


class WebSocket:
    """A client websocket on a verified stream; the websockets library does the protocol."""

    def __init__(self, reader, writer, protocol: ClientProtocol):
        self._reader = reader
        self._writer = writer
        self._protocol = protocol
        self._events: list = []
        self._fragments: list[bytes] = []
        self._fragment_text = False

    @classmethod
    async def connect(
        cls,
        url: str,
        pin: bytes | None,
        headers: list[tuple[str, str]] | None = None,
        max_size: int | None = 2**20,
        timeout: float = 10.0,
    ) -> WebSocket:
        endpoint = Endpoint.of(url)
        reader, writer = await open_stream(endpoint, pin, min(timeout, 3.0))
        protocol = ClientProtocol(parse_uri(url), max_size=max_size)
        socket = cls(reader, writer, protocol)
        try:
            async with asyncio.timeout(timeout):
                await socket._handshake(headers or [])
        except TimeoutError as err:
            await _close(writer)
            raise TransportError(f"websocket handshake timed out after {timeout:g}s") from err
        except BaseException:
            await _close(writer)
            raise
        return socket

    async def _handshake(self, headers) -> None:
        request = self._protocol.connect()
        for name, value in headers:
            request.headers[name] = value
        self._protocol.send_request(request)
        await self._flush()
        while True:
            if not await self._read_more():
                raise TransportError("connection closed during the websocket handshake")
            for event in self._protocol.events_received():
                if isinstance(event, WsResponse):
                    error = self._protocol.handshake_exc
                    if isinstance(error, InvalidStatus):
                        raise WsHttpError(error.response.status_code)
                    if error is not None:
                        raise TransportError(f"websocket handshake failed ({error})") from error
                    return
                self._events.append(event)

    async def _read_more(self) -> bool:
        """Feeds the protocol one read; False at end of stream."""
        try:
            data = await self._reader.read(READ_SIZE)
        except (OSError, ssl.SSLError) as err:
            raise TransportError(_describe_os_error(err)) from err
        if data:
            self._protocol.receive_data(data)
        else:
            self._protocol.receive_eof()
        await self._flush()
        return bool(data)

    async def _flush(self) -> None:
        # Pongs to the console's pings, and close frames, are queued by the protocol.
        data = self._protocol.data_to_send()
        if not data:
            return
        try:
            for chunk in data:
                if chunk:
                    self._writer.write(chunk)
                elif self._writer.can_write_eof():  # b"" asks for a half-close; TLS can't
                    self._writer.write_eof()
            await self._writer.drain()
        except (OSError, ssl.SSLError) as err:
            raise TransportError(_describe_os_error(err)) from err

    async def recv(self) -> str | bytes:
        """The next whole message; raises `WsClosed` once the connection is gone."""
        while True:
            while self._events:
                frame: Frame = self._events.pop(0)
                message = self._assemble(frame)
                if message is not None:
                    return message
            more = await self._read_more()
            self._events.extend(self._protocol.events_received())
            if not more and not self._events:
                raise WsClosed("closed by the console")

    def _assemble(self, frame: Frame) -> str | bytes | None:
        if frame.opcode is Opcode.CLOSE:
            raise WsClosed("closed by the console")
        if frame.opcode in (Opcode.TEXT, Opcode.BINARY):
            self._fragments = [bytes(frame.data)]
            self._fragment_text = frame.opcode is Opcode.TEXT
        elif frame.opcode is Opcode.CONT:
            self._fragments.append(bytes(frame.data))
        else:
            return None  # ping/pong: the protocol answers pings itself
        if not frame.fin:
            return None
        data = b"".join(self._fragments)
        self._fragments = []
        return data.decode() if self._fragment_text else data

    async def ping(self) -> None:
        self._protocol.send_ping(b"")
        await self._flush()

    async def close(self) -> None:
        try:
            if self._protocol.state.name == "OPEN":
                self._protocol.send_close()
                await self._flush()
        except TransportError:
            pass
        await _close(self._writer)
