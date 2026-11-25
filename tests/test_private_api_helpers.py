import unittest

from tests import support  # noqa: F401
from sauron import private_api
from sauron.net import Response


def response(*pairs: tuple[str, str]) -> Response:
    return Response(200, headers=[(name.encode(), value.encode()) for name, value in pairs], body=b"")


class HelpersTest(unittest.TestCase):
    def test_session_cookie_prefers_the_token_without_attributes(self):
        login = response(("set-cookie", "theme=dark; Path=/"),
                         ("set-cookie", "TOKEN=eyJ.test.sig; path=/; samesite=none; secure; httponly"))
        self.assertEqual(private_api.session_cookie(login), "TOKEN=eyJ.test.sig")

        uos = response(("set-cookie", "theme=dark"), ("set-cookie", "UOS_TOKEN=abc; Path=/"))
        self.assertEqual(private_api.session_cookie(uos), "UOS_TOKEN=abc")

        other = response(("set-cookie", "flag; Path=/"), ("set-cookie", "SESSION=xyz; Path=/"))
        self.assertEqual(private_api.session_cookie(other), "SESSION=xyz")
        self.assertIsNone(private_api.session_cookie(response()))

    def test_csrf_prefers_the_updated_token(self):
        both = response(("x-csrf-token", "old"), ("x-updated-csrf-token", "new"))
        self.assertEqual(private_api.csrf_header(both), "new")
        self.assertEqual(private_api.csrf_header(response(("x-csrf-token", "old"))), "old")
        self.assertIsNone(private_api.csrf_header(response()))

    def test_retry_after_takes_seconds_only(self):
        self.assertEqual(private_api.retry_after(response(("retry-after", " 120 "))), 120)
        self.assertIsNone(private_api.retry_after(response(("retry-after", "Wed, 21 Oct 2026 07:28:00 GMT"))))

    def test_quality_maps_to_livestream_channel(self):
        self.assertEqual([private_api.channel(q) for q in ("high", "medium", "low", "other")], [0, 1, 2, 0])


if __name__ == "__main__":
    unittest.main()
