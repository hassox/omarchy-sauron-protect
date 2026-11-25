import contextlib
import io
import unittest

from tests import support  # noqa: F401
from sauron import cli, event_log, history

# 2026-09-29T19:02:11.482Z, standing in for "now".
NOW = 1_790_708_531_482


class WhenTest(unittest.TestCase):
    def test_relative_times_count_back_from_now(self):
        self.assertEqual(history.parse_when("30m", NOW), NOW - 30 * 60_000)
        self.assertEqual(history.parse_when(" 24h ", NOW), NOW - 24 * 3_600_000)
        self.assertEqual(history.parse_when("7d", NOW), NOW - 7 * 86_400_000)
        self.assertEqual(history.parse_when("0m", NOW), NOW)

    def test_rfc3339_times_honour_their_offset(self):
        for text in [
            "2026-09-29T14:02:11.482-05:00",
            "2026-09-29T19:02:11.482Z",
            "2026-09-30T00:32:11.4829+05:30",
            "2026-09-29 19:02:11,482z",
        ]:
            with self.subTest(text=text):
                self.assertEqual(history.parse_when(text, 0), NOW)
        self.assertEqual(history.parse_when("2026-09-29T19:02:11.5Z", 0), NOW - 482 + 500)
        self.assertEqual(history.parse_when("2026-09-29T19:02Z", 0), NOW - 11_482)

    def test_local_times_read_as_the_local_clock(self):
        # Whatever the local zone: a local date or time reads back as the same wall-clock time.
        def local(text: str) -> str:
            return event_log.rfc3339(history.parse_when(text, NOW))

        self.assertTrue(local("2026-09-29").startswith("2026-09-29T00:00:00.000"), local("2026-09-29"))
        self.assertTrue(local("2026-09-29 10:30").startswith("2026-09-29T10:30:00.000"))
        self.assertTrue(local("2026-09-29t10:30:15").startswith("2026-09-29T10:30:15.000"))
        # And the event log's own timestamps parse back exactly.
        self.assertEqual(history.parse_when(event_log.rfc3339(NOW), 0), NOW)

    def test_malformed_times_are_errors(self):
        for text in [
            "", "30", "m", "-5m", "1.5h", "7w", "yesterday", "2026-9-29", "2026-13-01", "2026-09-32",
            "2026-09-29 24:00", "2026-09-29 10:60", "2026-09-29T10:00+05", "2026-09-29T10:00:00.Z",
            "2026-09-29T10:00:00+24:00", "2026-09-29T10:00Q",
        ]:
            with self.subTest(text=text):
                with self.assertRaises(ValueError) as caught:
                    history.parse_when(text, NOW)
                self.assertTrue(str(caught.exception).startswith("invalid time"), caught.exception)

    def test_range_defaults_until_to_now_and_rejects_since_after_until(self):
        self.assertEqual(history.time_range("24h", None, NOW), (NOW - 86_400_000, NOW))
        self.assertEqual(history.time_range("1h", "1h", NOW), (NOW - 3_600_000, NOW - 3_600_000))
        with self.assertRaises(ValueError) as caught:
            history.time_range("1h", "2h", NOW)
        message = str(caught.exception)
        self.assertTrue(message.startswith("--since ") and " is after --until " in message, message)
        with self.assertRaises(ValueError):
            history.time_range("1h", "soon", NOW)


class LogArgumentsTest(unittest.TestCase):
    def parse(self, args: list[str]):
        return cli._parser().parse_args(["log", *args])

    def test_log_arguments(self):
        args = self.parse(["--since=7d", "--until", "2026-09-29", "--all"])
        self.assertEqual((args.since, args.until, args.all), ("7d", "2026-09-29", True))
        args = self.parse([])
        self.assertEqual((args.since, args.until, args.all), ("24h", None, False))
        args = self.parse(["--since", "7d", "--all"])
        self.assertEqual((args.since, args.all), ("7d", True))

    def test_bad_log_arguments_are_usage_errors(self):
        for bad in [["--all=yes"], ["--since"], ["--bogus"], ["7d"]]:
            with self.subTest(args=bad), contextlib.redirect_stderr(io.StringIO()):
                with self.assertRaises(SystemExit) as caught:
                    self.parse(bad)
                self.assertEqual(caught.exception.code, 2)


NAMES = {"cam1": "Porch"}


def lines(events: list[dict], everything: bool) -> list[dict]:
    return [line for e in events if (line := history.line_for(e, NAMES, everything))]


class LineTest(unittest.TestCase):
    def assertLines(self, actual: list[dict], expected: list[dict]):
        self.assertEqual(actual, expected)
        # Key order is part of the output format.
        self.assertEqual([list(line) for line in actual], [list(line) for line in expected])

    def test_camera_events_use_the_event_log_shape(self):
        events = [
            {"id": "e1", "type": "smartDetectZone", "start": NOW, "end": NOW + 4_050, "camera": "cam1",
             "smartDetectTypes": ["person"], "score": 80},
            {"id": "e2", "type": "ring", "start": NOW, "end": None, "camera": "cam9", "smartDetectTypes": []},
        ]
        expected = [
            {"phase": "end", "id": "e1", "camera": "Porch", "camera_id": "cam1", "type": "smartDetectZone",
             "kinds": ["person"], "start": event_log.rfc3339(NOW), "end": event_log.rfc3339(NOW + 4_050),
             "duration_s": 4.1},
            {"phase": "start", "id": "e2", "camera": "cam9", "camera_id": "cam9", "type": "ring",
             "kinds": ["ring"], "start": event_log.rfc3339(NOW), "end": None, "duration_s": None},
        ]
        self.assertLines(lines(events, False), expected)
        self.assertLines(lines(events, True), expected)

    def test_other_events_only_with_all_and_keep_their_metadata(self):
        events = [
            {"id": "a1", "type": "access", "start": NOW, "end": None,
             "metadata": {"ip": "192.0.2.44", "n": [1, 2]}},
            {"id": "s1", "type": "sensorOpened", "start": NOW, "end": NOW + 1_000, "camera": "cam1"},
        ]
        self.assertEqual(lines(events, False), [])
        expected = [
            {"phase": "start", "id": "a1", "type": "access", "camera": None, "camera_id": None,
             "start": event_log.rfc3339(NOW), "end": None, "duration_s": None,
             "details": {"ip": "192.0.2.44", "n": [1, 2]}},
            {"phase": "end", "id": "s1", "type": "sensorOpened", "camera": "Porch", "camera_id": "cam1",
             "start": event_log.rfc3339(NOW), "end": event_log.rfc3339(NOW + 1_000), "duration_s": 1.0,
             "details": None},
        ]
        self.assertLines(lines(events, True), expected)


def serve(events: list[tuple[int, str]], start: int, end: int, limit: int) -> list[dict]:
    """What the console answers: events starting in [start, end], oldest first, at most `limit`."""
    page = [(at, id) for at, id in events if start <= at <= end][:limit]
    return [{"id": id, "type": "motion", "start": at, "camera": "cam1"} for at, id in page]


def walk(events: list[tuple[int, str]], since: int, until: int, limit: int) -> list[str]:
    """The ids `sauron log` prints walking `events` a page of `limit` at a time."""
    pager = history.Pager(since, until)
    printed = []
    for _ in range(100):
        page = serve(events, pager.cursor, until, limit)
        printed += [e["id"] for e in page if pager.is_new(e)]
        if not pager.advance(page, limit):
            return printed
    raise AssertionError("the walk never ended")


class PagerTest(unittest.TestCase):
    def test_paging_prints_each_event_once_across_shared_milliseconds(self):
        events = [(100, "a"), (200, "b"), (300, "c"), (300, "d"), (400, "e"), (400, "f"), (500, "g"),
                  (600, "h")]
        for limit in [1, 2, 3, 4, 8, 9]:
            # With a page of one, a millisecond shared by two events is stepped past after the first.
            expected = list("abcegh") if limit == 1 else list("abcdefgh")
            self.assertEqual(walk(events, 100, 1_000, limit), expected, f"limit {limit}")

    def test_paging_keeps_to_the_range_and_steps_past_a_full_millisecond(self):
        events = [(100, "a"), (200, "b"), (300, "c"), (300, "d"), (300, "e"), (400, "f"), (500, "g")]
        self.assertEqual(walk(events, 200, 400, 3), ["b", "c", "d", "e", "f"])
        # A page entirely from 300 moves the cursor to 301 instead of asking for 300 forever.
        self.assertEqual(walk(events, 300, 1_000, 3), ["c", "d", "e", "f", "g"])


if __name__ == "__main__":
    unittest.main()
