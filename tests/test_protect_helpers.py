import unittest

from tests import support  # noqa: F401
from sauron import net, protect
from sauron.protect import Attempt, AuthError, BadResponse, Camera, ConnectError, HttpError

NO_ANSWER = net.TransportError("Connection refused")
MISMATCH = net.Rejection("mismatch", b"leaf")
UNTRUSTED = net.Rejection("untrusted", b"leaf", "unable to get local issuer certificate")


def attempts(*pairs: tuple[str, BaseException]) -> ConnectError:
    return ConnectError([Attempt(address, error) for address, error in pairs])


class StreamUrlTest(unittest.TestCase):
    def test_stream_url_must_be_rtsp(self):
        # The URL goes straight to the player, which would open anything.
        for url in ["file:///etc/passwd", "http://10.0.0.5/x", "-i"]:
            with self.subTest(url=url), self.assertRaises(ValueError):
                protect.fixup_stream_url(url, "192.0.2.10")

    def test_stream_url_loses_srtp_and_points_at_the_console(self):
        self.assertEqual(protect.fixup_stream_url("rtsps://10.0.0.5:7441/aBcToKeN123?enableSrtp", "192.0.2.10"),
                         "rtsps://192.0.2.10:7441/aBcToKeN123")
        self.assertEqual(protect.fixup_stream_url("rtsps://10.0.0.5:7441/tok?enableSrtp&foo=bar", "unifi.example"),
                         "rtsps://unifi.example:7441/tok?foo=bar")
        self.assertEqual(protect.fixup_stream_url("rtsps://10.0.0.5/tok", "192.0.2.10"),
                         "rtsps://192.0.2.10/tok")


class ConnectErrorTest(unittest.TestCase):
    def test_offline_message_joins_every_address(self):
        err = attempts(("192.0.2.10", NO_ANSWER), ("unifi.example", MISMATCH))
        self.assertEqual(err.failure, "offline")
        self.assertEqual(str(err),
                         "192.0.2.10: no answer · unifi.example: not your console (certificate mismatch)")

    def test_auth_beats_untrusted_which_beats_offline(self):
        auth = attempts(("192.0.2.10", UNTRUSTED), ("198.51.100.7", NO_ANSWER), ("unifi.example", AuthError()))
        self.assertEqual(auth.failure, "auth")
        self.assertEqual(str(auth), "API key rejected (401)")

        untrusted = attempts(("192.0.2.10", NO_ANSWER), ("unifi.example", UNTRUSTED))
        self.assertEqual(untrusted.failure, "untrusted")
        self.assertEqual(str(untrusted), protect.PIN_HINT)

    def test_offline_message_is_cut_to_120_characters(self):
        message = str(attempts(*[("very-long-hostname.unifi.example", NO_ANSWER)] * 6))
        self.assertEqual(len(message), protect.SUMMARY_MAX)
        self.assertTrue(message.startswith("very-long-hostname.unifi.example: no answer · "))
        self.assertTrue(message.endswith("…"))

        self.assertEqual(protect.truncate("é" * 120, 120), "é" * 120)
        self.assertEqual(protect.truncate("é" * 121, 120), "é" * 119 + "…")


class OutcomeTest(unittest.TestCase):
    def test_outcome_classifies_certificate_failures(self):
        self.assertEqual(protect.outcome(MISMATCH), "not your console (certificate mismatch)")
        self.assertEqual(protect.outcome(UNTRUSTED), "untrusted certificate")
        handshake = net.TransportError("TLS handshake failed (bad record)")
        self.assertEqual(protect.outcome(handshake), "TLS handshake failed")

    def test_outcome_classifies_other_failures(self):
        self.assertEqual(protect.outcome(NO_ANSWER), "no answer")
        self.assertEqual(protect.outcome(AuthError()), "API key rejected")
        http = HttpError("/v1/cameras", 404)
        self.assertEqual(protect.outcome(http), "HTTP 404")
        self.assertEqual(protect.failure_message("192.0.2.10", http), "192.0.2.10: HTTP 404")
        self.assertEqual(protect.outcome(BadResponse("/v1/cameras: unexpected response")),
                         "unexpected response")


class CameraTest(unittest.TestCase):
    def test_camera_kinds_follow_feature_flags_and_model(self):
        camera = Camera.from_json({
            "id": "cam", "type": "UVC G4 Doorbell Pro",
            "featureFlags": {"smartDetectTypes": ["person", "package"], "smartDetectAudioTypes": ["alrmSmoke"]},
        })
        self.assertEqual(camera.kinds(), ["person", "package", "audio", "motion", "ring"])
        plain = Camera.from_json({"id": "cam", "type": "UVC G3", "featureFlags": {"smartDetectAudioTypes": []}})
        self.assertEqual(plain.kinds(), ["motion"])

    def test_display_name_falls_back_to_model_then_id(self):
        self.assertEqual(Camera("cam-id", name="Porch", model="G4").display_name, "Porch")
        self.assertEqual(Camera("cam-id", name="  ", model="G4").display_name, "G4")
        self.assertEqual(Camera("cam-id").display_name, "cam-id")


CAMERAS = [Camera("cam-a", name="Garage"), Camera("cam-b", name="Garage Side"), Camera("cam-c", name="Front Door")]


class ResolveTest(unittest.TestCase):
    def test_resolve_by_id_exact_name_then_unique_prefix(self):
        def resolved(query: str) -> str:
            return protect.resolve(CAMERAS, query).id

        self.assertEqual(resolved("cam-c"), "cam-c")
        # An exact name wins although it is also a prefix of another camera's name.
        self.assertEqual(resolved("gARAGE"), "cam-a")
        self.assertEqual(resolved("fro"), "cam-c")
        self.assertEqual(resolved("garage s"), "cam-b")

    def test_resolve_reports_ambiguous_and_unknown_queries(self):
        def error(query: str) -> str:
            with self.assertRaises(LookupError) as caught:
                protect.resolve(CAMERAS, query)
            return caught.exception.args[0]

        self.assertEqual(error("Gar"), '"gar" is ambiguous: Garage, Garage Side')
        self.assertEqual(error("back"), 'no camera matches "back"')


if __name__ == "__main__":
    unittest.main()
