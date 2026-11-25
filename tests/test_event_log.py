import json
import os
import stat
import tempfile
import time
import unittest
from pathlib import Path

from tests import support  # noqa: F401
from sauron import event_log

# 2026-09-29T19:02:11.482Z
MS = 1_790_708_531_482


class LocalZone:
    """Switches the process's local zone; rfc3339 formats local time, whatever zone the host is in."""

    def setUp(self):
        self._tz = os.environ.get("TZ")

    def tearDown(self):
        if self._tz is None:
            os.environ.pop("TZ", None)
        else:
            os.environ["TZ"] = self._tz
        time.tzset()

    def zone(self, tz: str) -> None:
        os.environ["TZ"] = tz
        time.tzset()


class TimeTest(LocalZone, unittest.TestCase):
    def at(self, ms: int, tz: str) -> str:
        self.zone(tz)
        return event_log.rfc3339(ms)

    def test_times_are_rfc3339_with_millis_and_offset(self):
        # POSIX TZ offsets count west of UTC, so EST5 is -05:00.
        self.assertEqual(self.at(MS, "EST5"), "2026-09-29T14:02:11.482-05:00")
        self.assertEqual(self.at(MS, "UTC0"), "2026-09-29T19:02:11.482+00:00")
        self.assertEqual(self.at(MS, "IST-5:30"), "2026-09-30T00:32:11.482+05:30")
        self.assertEqual(self.at(MS, "MART9:30"), "2026-09-29T09:32:11.482-09:30")
        # 2026-03-01T04:30:00.007Z: back across a month end, millis zero-padded.
        self.assertEqual(self.at(1_772_339_400_007, "EST5"), "2026-02-28T23:30:00.007-05:00")
        # Before the epoch the millis still count forward within the second.
        self.assertEqual(self.at(-1, "UTC0"), "1969-12-31T23:59:59.999+00:00")


class LineTest(LocalZone, unittest.TestCase):
    def setUp(self):
        super().setUp()
        self.zone("EST5")

    def test_duration_is_rounded_to_a_tenth_and_never_negative(self):
        self.assertEqual(event_log.duration_s(1_000, 2_049), 1.0)
        self.assertEqual(event_log.duration_s(1_000, 2_050), 1.1)
        # Halves round up, as in the Rust daemon, not to even.
        self.assertEqual(event_log.duration_s(0, 4_050), 4.1)
        self.assertEqual(event_log.duration_s(0, 250), 0.3)
        self.assertEqual(event_log.duration_s(0, 90_000), 90.0)
        self.assertEqual(event_log.duration_s(5_000, 4_000), 0.0)

    def test_kinds_are_normalized_per_event_type(self):
        detected = ["person", "vehicle"]
        self.assertEqual(event_log.event_kinds("ring", detected), ["ring"])
        self.assertEqual(event_log.event_kinds("motion", detected), ["motion"])
        self.assertEqual(event_log.event_kinds("smartAudioDetect", ["alrmSmoke"]), ["audio"])
        self.assertEqual(event_log.event_kinds("smartDetectZone", detected), detected)
        self.assertEqual(event_log.event_kinds("smartDetectLine", None), [])

    def test_start_line_has_null_end_and_omits_match_and_forced(self):
        line = event_log.camera_line("start", "66f9a1b2c3d4e5f601234567", "Porch", "cam1",
                                     "smartDetectZone", ["person"], MS, None)
        expected = {"phase": "start", "id": "66f9a1b2c3d4e5f601234567", "camera": "Porch",
                    "camera_id": "cam1", "type": "smartDetectZone", "kinds": ["person"],
                    "start": "2026-09-29T14:02:11.482-05:00", "end": None, "duration_s": None}
        self.assertEqual(line, expected)
        self.assertEqual(list(line), list(expected))

    def test_end_line_carries_match_duration_and_forced(self):
        line = event_log.camera_line("end", "66f9a1b2c3d4e5f601234567", "Porch", "cam1",
                                     "smartDetectZone", ["person", "vehicle"], MS, MS + 12_345,
                                     matched=False, forced=True)
        expected = {"phase": "end", "id": "66f9a1b2c3d4e5f601234567", "camera": "Porch",
                    "camera_id": "cam1", "type": "smartDetectZone", "kinds": ["person", "vehicle"],
                    "match": False, "start": "2026-09-29T14:02:11.482-05:00",
                    "end": "2026-09-29T14:02:23.827-05:00", "duration_s": 12.3, "forced": True}
        self.assertEqual(line, expected)
        self.assertEqual(list(line), list(expected))


def mode(path: Path) -> int:
    return stat.S_IMODE(path.stat().st_mode)


class AppendTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self.tmp.name)

    def tearDown(self):
        self.tmp.cleanup()

    def test_append_writes_a_private_file_and_its_directory(self):
        path = self.dir / "state/sauron/events.jsonl"
        event_log.append(path, b"a\n")
        event_log.append(path, b"b\n")
        self.assertEqual(path.read_bytes(), b"a\nb\n")
        self.assertEqual(mode(path), 0o600)
        self.assertEqual(mode(path.parent), 0o700)

    def test_append_starts_a_fresh_file_after_rotation(self):
        path = self.dir / "events.jsonl"
        rotated = self.dir / "events.jsonl.1"
        event_log.append(path, b"old\n")
        path.rename(rotated)
        event_log.append(path, b"new\n")
        self.assertEqual(rotated.read_bytes(), b"old\n")
        self.assertEqual(path.read_bytes(), b"new\n")
        self.assertEqual(mode(path), 0o600)


class WriterTest(unittest.IsolatedAsyncioTestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self.tmp.name)
        self.warnings: list[str] = []

    def tearDown(self):
        self.tmp.cleanup()

    async def write_all(self, lines: list[tuple[Path, str]]) -> None:
        writer = event_log.Writer(self.warnings.append)
        for path, text in lines:
            writer.write(path, {"line": text})
        await writer.flush()

    async def test_writer_appends_lines_in_order_to_a_private_file(self):
        path = self.dir / "state/sauron/events.jsonl"
        await self.write_all([(path, "a"), (path, "b")])
        await self.write_all([(path, "c")])
        self.assertEqual(self.warnings, [])
        # The README documents compact JSON Lines.
        self.assertEqual(path.read_text(), '{"line":"a"}\n{"line":"b"}\n{"line":"c"}\n')
        self.assertEqual(mode(path), 0o600)

    async def test_writer_reports_each_error_once_per_log_path(self):
        (self.dir / "file").write_bytes(b"")
        broken = self.dir / "file/events.jsonl"
        good = self.dir / "events.jsonl"
        await self.write_all([(broken, "a"), (broken, "b"), (good, "c"), (broken, "d")])
        self.assertEqual(len(self.warnings), 2, self.warnings)
        for warning in self.warnings:
            self.assertTrue(warning.startswith(f"event log {broken}: "), warning)
        self.assertEqual(json.loads(good.read_text()), {"line": "c"})


if __name__ == "__main__":
    unittest.main()
