import json
import unittest

from tests import support  # noqa: F401
from sauron import notify

LIVE_ARGV = ["/usr/bin/python3", "/opt/sauron/bin/sauron", "live"]
# 2026-09-29T19:02:11.482Z
MS = 1_790_708_531_482


class SummaryTest(unittest.TestCase):
    def test_summary_names_kinds_then_camera(self):
        self.assertEqual(notify.summary(["person", "licensePlate", "audio", "smokeAlarm"], "Driveway"),
                         "Person, License plate, Sound, SmokeAlarm · Driveway")


class CommandTest(unittest.TestCase):
    def setUp(self):
        self.notifier = notify.Notifier(LIVE_ARGV)

    def command(self, camera_name="Porch", image=None, event_id="ev"):
        return self.notifier.command(event_id, "cam1", camera_name, ["person"], MS, image)

    def hint(self, argv: list[str], name: str) -> str | None:
        prefix = f"--hint=string:{name}:"
        values = [arg[len(prefix):] for arg in argv if arg.startswith(prefix)]
        self.assertLessEqual(len(values), 1, argv)
        return values[0] if values else None

    def test_command_prints_the_id_and_carries_the_omarchy_hints(self):
        argv = self.command()
        self.assertEqual(argv[0], "notify-send")
        self.assertIn("--print-id", argv)
        self.assertEqual(self.hint(argv, "omarchy-glyph"), notify.GLYPH)
        # Clicking the notification opens that camera's live view.
        self.assertEqual(json.loads(self.hint(argv, "omarchy-exec-argv")), [*LIVE_ARGV, "cam1"])

    def test_image_hint_only_with_an_image(self):
        self.assertIsNone(self.hint(self.command(), "image-path"))
        argv = self.command(image="/run/user/1000/sauron/cam1.jpg")
        self.assertEqual(self.hint(argv, "image-path"), "/run/user/1000/sauron/cam1.jpg")

    def test_replace_id_only_for_an_event_already_notified(self):
        self.assertFalse(any(arg.startswith("--replace-id") for arg in self.command()))
        self.notifier._ids["ev"] = 7
        self.assertIn("--replace-id=7", self.command())
        self.assertFalse(any(arg.startswith("--replace-id") for arg in self.command(event_id="other")))

    def test_camera_name_cannot_become_an_option(self):
        argv = self.command(camera_name="--evil")
        separator = argv.index("--")
        self.assertEqual(argv[separator + 1], "Person · --evil")
        self.assertEqual(len(argv), separator + 3)
        self.assertFalse(any("--evil" in arg for arg in argv[:separator]))


if __name__ == "__main__":
    unittest.main()
