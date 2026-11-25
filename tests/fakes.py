"""Stand-ins for the desktop services the daemon talks to: the keyring and notifications."""

import asyncio
import json
import os
import tempfile
from contextlib import ExitStack
from pathlib import Path
from unittest import mock


class Keyring:
    """An in-memory secret store patched over sauron.keyring."""

    def __init__(self):
        self.secrets: dict[tuple[str, str], str] = {}

    async def lookup(self, console, username):
        return self.secrets.get((console, username))

    async def store(self, console, username, password):
        self.secrets[(console, username)] = password

    def patch(self, stack: ExitStack) -> "Keyring":
        stack.enter_context(mock.patch("sauron.keyring.lookup", self.lookup))
        stack.enter_context(mock.patch("sauron.keyring.store", self.store))
        return self


class Output:
    """Collects the daemon's JSON lines and lets a test wait for one."""

    def __init__(self):
        self.text = ""
        self.messages: list[dict] = []
        self._changed = asyncio.Event()

    def write(self, text):
        self.text += text
        for line in text.splitlines():
            self.messages.append(json.loads(line))
        self._changed.set()

    def flush(self):
        pass

    async def wait_for(self, predicate, timeout=5.0, after=0):
        """The first message (from index `after`) that satisfies `predicate`."""
        async with asyncio.timeout(timeout):
            while True:
                for message in self.messages[after:]:
                    if predicate(message):
                        return message
                self._changed.clear()
                await self._changed.wait()

    def of(self, kind):
        return [m for m in self.messages if m.get("t") == kind]


class Environment:
    """A private config and runtime folder for one test, via SAURON_CONFIG/SAURON_RUNTIME_DIR."""

    def __init__(self, stack: ExitStack):
        self.dir = Path(stack.enter_context(tempfile.TemporaryDirectory(prefix="sauron-test-")))
        self.config = self.dir / "config" / "config.toml"
        self.runtime = self.dir / "run"
        stack.enter_context(mock.patch.dict(os.environ, {
            "SAURON_CONFIG": str(self.config), "SAURON_RUNTIME_DIR": str(self.runtime)}))
