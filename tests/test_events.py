"""The daemon's event state machine: what Protect's frames turn into."""

import unittest

from tests import support  # noqa: F401
from sauron.daemon import ENDED_REMEMBERED, EVENT_TTL, Events, Filter

CAMERAS = {"cam-a", "cam-b"}


def full(event_id, event_type="smartDetectZone", camera="cam-a", types=("person",), start=1000, end=None):
    item = {"id": event_id, "type": event_type, "start": start, "device": camera,
            "smartDetectTypes": list(types)}
    if end is not None:
        item["end"] = end
    return item


class Harness:
    def __init__(self, filter=None):
        self.events = Events()
        self.filter = filter or Filter(notify=["person", "vehicle"])
        self.now = 0.0

    def send(self, action, item):
        return self.events.apply(action, item, self.filter, lambda c: c in CAMERAS, self.now)

    def lines(self, action, item):
        return [(c.phase, c.id, c.event.kinds, c.end, c.notify) for c in self.send(action, item)]


class EventsTest(unittest.TestCase):
    def test_add_starts_and_update_with_end_ends(self):
        h = Harness()
        self.assertEqual(h.lines("add", full("e1")), [("start", "e1", ["person"], None, True)])
        self.assertEqual(h.lines("update", {"id": "e1", "end": 2000}),
                         [("end", "e1", ["person"], 2000, False)])
        self.assertEqual(h.events.open, {})

    def test_partial_update_merges_into_the_open_event(self):
        h = Harness()
        h.send("add", full("e1", types=["animal"]))
        # Kinds grew to include one the filter wants: notify now, as an update.
        self.assertEqual(h.lines("update", {"id": "e1", "smartDetectTypes": ["animal", "person"]}),
                         [("update", "e1", ["animal", "person"], None, True)])
        # Same kinds again: nothing to say.
        self.assertEqual(h.lines("update", {"id": "e1", "smartDetectTypes": ["animal", "person"]}), [])
        # A kind the filter already matched doesn't notify twice.
        self.assertEqual(h.lines("update", {"id": "e1", "smartDetectTypes": ["person"]}),
                         [("update", "e1", ["person"], None, False)])

    def test_complete_event_for_an_unseen_id_starts_and_ends(self):
        h = Harness()
        self.assertEqual(h.lines("update", full("e1", end=1500)),
                         [("start", "e1", ["person"], None, True), ("end", "e1", ["person"], 1500, False)])

    def test_frames_after_the_end_are_ignored(self):
        h = Harness()
        h.send("add", full("e1"))
        h.send("update", {"id": "e1", "end": 2000})
        self.assertEqual(h.send("update", {"id": "e1", "smartDetectTypes": ["vehicle"]}), [])
        self.assertEqual(h.send("update", full("e1", end=3000)), [])

    def test_frames_that_cannot_start_an_event_are_dropped(self):
        h = Harness()
        self.assertEqual(h.send("add", {"id": "e1"}), [], "missing type/start/device")
        self.assertEqual(h.send("update", full("e2")), [], "update without an end for an unseen id")
        self.assertEqual(h.send("remove", full("e3")), [], "other actions")
        self.assertEqual(h.send("add", full("e4", event_type="access")), [], "not a camera event type")
        self.assertEqual(h.send("add", full("e5", camera="cam-unknown")), [], "not one of our cameras")
        self.assertEqual(h.send("add", {"type": "motion", "start": 1, "device": "cam-a"}), [], "no id")

    def test_notifications_follow_the_filter(self):
        h = Harness(Filter(notify=["vehicle"], cameras=["cam-b"]))
        self.assertFalse(h.send("add", full("e1", camera="cam-a", types=["vehicle"]))[0].notify)
        self.assertFalse(h.send("add", full("e2", camera="cam-b", types=["person"]))[0].notify)
        self.assertTrue(h.send("add", full("e3", camera="cam-b", types=["vehicle"]))[0].notify)
        self.assertEqual(h.send("add", full("e4", event_type="ring", camera="cam-b"))[0].event.kinds, ["ring"])

    def test_ended_ids_are_remembered_up_to_a_bound(self):
        h = Harness()
        for i in range(ENDED_REMEMBERED + 1):
            h.send("update", full(f"e{i}", end=2000))
        self.assertEqual(len(h.events.ended), ENDED_REMEMBERED)
        self.assertNotIn("e0", h.events.ended)
        # The oldest was forgotten, so a late complete frame for it starts it again.
        self.assertEqual(h.send("update", full("e0", end=2000))[0].phase, "start")

    def test_stale_events_and_disconnects_force_the_end(self):
        h = Harness()
        h.send("add", full("old"))
        h.now = EVENT_TTL - 1
        h.send("add", full("young"))
        h.now = EVENT_TTL + 1
        stale = h.events.end_stale(h.now, 9000)
        self.assertEqual([(c.id, c.phase, c.end, c.forced) for c in stale], [("old", "end", 9000, True)])
        rest = h.events.end_all(9500)
        self.assertEqual([(c.id, c.forced) for c in rest], [("young", True)])
        self.assertEqual(h.events.open, {})


if __name__ == "__main__":
    unittest.main()
