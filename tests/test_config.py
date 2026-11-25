import os
import tempfile
import unittest
from dataclasses import replace
from pathlib import Path
from unittest import mock

from tests import support  # noqa: F401
from sauron import config, net
from sauron.config import Invalid, NotSetUp, Settings

PIN = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"


def settings(**fields) -> Settings:
    return replace(Settings(host="192.0.2.10", api_key="test-key"), **fields)


def parse(text: str) -> Settings:
    with tempfile.TemporaryDirectory() as directory:
        path = Path(directory) / "config.toml"
        path.write_text(text)
        return config.read_settings(path)


class ValidateTest(unittest.IsolatedAsyncioTestCase):
    async def invalid(self, settings: Settings) -> str:
        with self.assertRaises(Invalid) as caught:
            await config.validate(settings)
        return str(caught.exception)

    async def test_validate_normalizes_addresses_pin_and_key(self):
        validated = await config.validate(Settings(
            host=" https://192.0.2.10:8443/ ",
            fallback_hosts=["", " 198.51.100.7/ ", "  "],
            cert_sha256=f" {PIN} ",
            api_key="  test-key \n",
            username=" viewer ",
            event_log=" /var/tmp/sauron events.jsonl ",
        ))
        self.assertEqual(validated.host, "https://192.0.2.10:8443")
        self.assertEqual(validated.fallback_hosts, ("198.51.100.7",))
        self.assertEqual(validated.pin, net.parse_fingerprint(PIN))
        self.assertEqual(validated.api_key, "test-key")
        self.assertEqual(validated.username, "viewer")
        self.assertEqual(validated.event_log, Path("/var/tmp/sauron events.jsonl"))

    async def test_empty_host_is_reported_first(self):
        message = await self.invalid(Settings(host=" / ", live_quality="ultra"))
        self.assertEqual(message, "Config error: `host` is empty")

    async def test_bad_values_are_errors_even_without_a_key(self):
        cases = [
            (dict(cert_sha256="AB:CD"), "`cert_sha256`"),
            (dict(live_quality="ultra"), "`live_quality`"),
            (dict(player=[]), "`player`"),
            (dict(player=["", "--flag"]), "`player`"),
            (dict(fallback_hosts=["ftp://198.51.100.7"]), "http://"),
            (dict(host="ftp://192.0.2.10"), "http://"),
        ]
        for fields, needle in cases:
            with self.subTest(fields=fields):
                self.assertIn(needle, await self.invalid(settings(api_key="", **fields)))

    async def test_exactly_one_key_source_is_required(self):
        with self.assertRaises(NotSetUp):
            await config.validate(settings(api_key="  ", api_key_command=" "))
        self.assertIn("only one of", await self.invalid(settings(api_key_command="printf other")))

    async def test_key_command_output_is_trimmed_and_failures_reported(self):
        def command(text: str) -> Settings:
            return settings(api_key="", api_key_command=text)

        validated = await config.validate(command("printf '  cmd-key\\n'"))
        self.assertEqual(validated.api_key, "cmd-key")
        message = await self.invalid(command("echo locked >&2; exit 3"))
        self.assertIn("failed", message)
        self.assertIn("locked", message)
        self.assertIn("printed nothing", await self.invalid(command("true")))

    async def test_event_log_is_absolute_or_under_home(self):
        self.assertIn("absolute path", await self.invalid(settings(event_log="logs/events.jsonl")))
        self.assertIn("absolute path", await self.invalid(settings(event_log="~alice/events.jsonl")))
        self.assertIsNone((await config.validate(settings(event_log="  "))).event_log)

        with mock.patch.dict(os.environ, {"HOME": "/home/viewer"}):
            validated = await config.validate(settings(event_log="~/sauron/events.jsonl"))
        self.assertEqual(validated.event_log, Path("/home/viewer/sauron/events.jsonl"))

        # Without HOME, `~/` can't be expanded, and saying so beats guessing a directory.
        with mock.patch.dict(os.environ):
            os.environ.pop("HOME", None)
            self.assertIn("HOME", await self.invalid(settings(event_log="~/sauron/events.jsonl")))


class RenderTest(unittest.TestCase):
    def test_render_round_trips_through_the_parser(self):
        full = Settings(
            host="unifi.example",
            fallback_hosts=["198.51.100.7", "https://unifi.example:8443"],
            cert_sha256=net.fingerprint_text(net.parse_fingerprint(PIN)),
            api_key='k"e\\y#1 ünï',
            api_key_command='secret-tool lookup service "sauron" # not a comment',
            username="viewer",
            live_quality="low",
            player=["mpv", '--title="x"', "--geometry=50%"],
            event_log="~/.local/state/sauron/events.jsonl",
        )
        self.assertEqual(parse(config.render(full)), full)
        self.assertEqual(parse(config.render(Settings())), Settings())

    def test_render_writes_canonical_address_and_pin(self):
        parsed = parse(config.render(settings(
            host=" 192.0.2.10/ ", fallback_hosts=[" ", "198.51.100.7/"], cert_sha256=PIN)))
        self.assertEqual(parsed.host, "192.0.2.10")
        self.assertEqual(parsed.fallback_hosts, ["198.51.100.7"])
        self.assertEqual(parsed.cert_sha256, net.fingerprint_text(net.parse_fingerprint(PIN)))


class ParseTest(unittest.TestCase):
    def test_old_verify_tls_key_still_parses(self):
        parsed = parse('host = "192.0.2.10"\nverify_tls = false\napi_key = "k"\n')
        self.assertEqual((parsed.host, parsed.api_key), ("192.0.2.10", "k"))
        self.assertEqual(parsed.live_quality, "high")

    def test_parse_errors_name_the_line_on_one_line(self):
        # tomllib words its errors differently from the Rust parser; the line number is what counts.
        with self.assertRaises(Invalid) as caught:
            parse('host = "192.0.2.10"\napi_key = "k"\nlive_quality = high\n')
        message = str(caught.exception)
        self.assertIn("line 3", message)
        self.assertNotIn("\n", message)

    def test_missing_file_is_none(self):
        self.assertIsNone(config.read_settings(Path("/nonexistent/sauron/config.toml")))

    def test_default_gateway_is_the_first_default_route_with_a_gateway(self):
        header = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n"
        table = (header
                 + "wg0\t00000000\t00000000\t0001\t0\t0\t50\t00000000\t0\t0\t0\n"
                 + "wlan0\t0000A8C0\t00000000\t0001\t0\t0\t600\t00FFFFFF\t0\t0\t0\n"
                 + "wlan0\t00000000\t0100A8C0\t0003\t0\t0\t600\t00000000\t0\t0\t0\n"
                 + "eth0\t00000000\t0101A8C0\t0003\t0\t0\t700\t00000000\t0\t0\t0\n")
        self.assertEqual(config.gateway_from_routes(table), "192.168.0.1")
        no_default = header + "wlan0\t0000A8C0\t00000000\t0001\t0\t0\t600\t00FFFFFF\t0\t0\t0\n"
        self.assertIsNone(config.gateway_from_routes(no_default))


class AddressTest(unittest.TestCase):
    def test_base_url_defaults_to_https_and_the_api_path(self):
        self.assertEqual(config.base_url("192.0.2.10"), "https://192.0.2.10/proxy/protect/integration")
        self.assertEqual(config.base_url("unifi.example:8443"),
                         "https://unifi.example:8443/proxy/protect/integration")
        # An explicit URL keeps its scheme, port and any path.
        self.assertEqual(config.base_url("http://192.0.2.10:8080"),
                         "http://192.0.2.10:8080/proxy/protect/integration")
        self.assertEqual(config.base_url("https://192.0.2.10/custom/api"), "https://192.0.2.10/custom/api")

    def test_base_url_rejects_other_schemes_and_garbage(self):
        with self.assertRaisesRegex(ValueError, "http:// or https://"):
            config.base_url("ftp://192.0.2.10")
        with self.assertRaisesRegex(ValueError, "invalid host"):
            config.base_url("not a host")

    def test_console_id_is_host_and_explicit_port(self):
        self.assertEqual(config.console_id("192.0.2.10"), "192.0.2.10")
        self.assertEqual(config.console_id("https://unifi.example:7443/proxy/protect/integration"),
                         "unifi.example:7443")


if __name__ == "__main__":
    unittest.main()
