"""Shared test fixtures: a throwaway certificate and the fake console."""

import logging
import ssl
import sys
import tempfile
from functools import cache
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))
sys.path.insert(0, str(ROOT / "tools"))

import sauron  # noqa: E402,F401  (puts the vendored libraries on sys.path)
import mock_protect  # noqa: E402
from sauron import net  # noqa: E402

# The fake console logs every handshake a test aborts on purpose.
logging.getLogger("asyncio").setLevel(logging.CRITICAL)


@cache
def certificate() -> tuple[Path, Path]:
    """A self-signed certificate and key shared by every test in the run."""
    directory = Path(tempfile.mkdtemp(prefix="sauron-test-"))
    return mock_protect.self_signed(directory)


def server_tls() -> ssl.SSLContext:
    return mock_protect.server_context(*certificate())


def leaf_der() -> bytes:
    pem = certificate()[0].read_text()
    return ssl.PEM_cert_to_DER_cert(pem)


def pin() -> bytes:
    return net.sha256(leaf_der())


async def console(**kwargs) -> "mock_protect.MockConsole":
    kwargs.setdefault("tls", server_tls())
    return await mock_protect.MockConsole(**kwargs).start()
