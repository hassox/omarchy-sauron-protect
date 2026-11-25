import unittest

from tests import support  # noqa: F401
from sauron import live
from sauron.live import AUDIO, BEGIN, END, INIT, MDAT, MOOF, TIMESTAMP, VIDEO


def frame(kind: int, payload: bytes) -> bytes:
    return bytes([kind]) + len(payload).to_bytes(3, "big") + payload


def segment(tag: str) -> bytes:
    """A media segment whose frames arrive in a different order than they must be written."""
    return b"".join([
        frame(BEGIN, b""),
        frame(TIMESTAMP, b"\x00\x00\x00\x2a"),
        frame(AUDIO, f"audio{tag}".encode()),
        frame(VIDEO, f"video{tag}".encode()),
        frame(MDAT, f"mdat{tag}".encode()),
        frame(MOOF, f"moof{tag}".encode()),
        frame(END, b""),
    ])


def demux(messages: list[bytes]) -> bytes:
    demuxer = live.Demux()
    return b"".join(demuxer.push(message) for message in messages)


STREAM = frame(INIT, b"INIT") + segment("1") + segment("2")


class DemuxTest(unittest.TestCase):
    def test_writes_init_then_each_segment_as_moof_mdat_video_audio(self):
        self.assertEqual(demux([STREAM]), b"INITmoof1mdat1video1audio1moof2mdat2video2audio2")

    def test_frames_may_span_websocket_messages(self):
        whole = demux([STREAM])
        for size in [1, 3, 5, 17]:
            chunks = [STREAM[i:i + size] for i in range(0, len(STREAM), size)]
            self.assertEqual(demux(chunks), whole, f"chunks of {size}")

        # Nothing is written until the segment's end frame is complete.
        demuxer = live.Demux()
        self.assertEqual(demuxer.push(STREAM[:-5]), b"INITmoof1mdat1video1audio1")
        self.assertEqual(demuxer.push(STREAM[-5:]), b"moof2mdat2video2audio2")

    def test_init_is_written_once_and_media_waits_for_it(self):
        stream = (segment("0") + frame(INIT, b"INIT") + segment("1") + frame(INIT, b"AGAIN")
                  + segment("2"))
        self.assertEqual(demux([stream]), b"INITmoof1mdat1video1audio1moof2mdat2video2audio2")

    def test_begin_discards_a_partial_segment_and_unknown_frames_are_skipped(self):
        stream = frame(INIT, b"INIT") + frame(MOOF, b"stale") + frame(248, b"avc1.640028") + segment("1")
        self.assertEqual(demux([stream]), b"INITmoof1mdat1video1audio1")

    def test_lost_sync_drops_the_message_and_recovers_on_the_next(self):
        # Not a frame type: its "length" would otherwise stall the stream waiting for 16 MiB.
        garbage = frame(MOOF, b"half") + bytes([0x01, 0xFF, 0xFF, 0xFF, 0x00])
        out = demux([frame(INIT, b"INIT"), garbage, segment("1")])
        self.assertEqual(out, b"INITmoof1mdat1video1audio1")


URL = "rtsps://192.0.2.10:7441/aBcToKeN123"


class PlayerCommandTest(unittest.TestCase):
    def test_mpv_gets_the_stream_url_on_stdin_not_in_its_arguments(self):
        argv, url_on_stdin = live.player_command(["mpv", "--untimed"], "Sauron", URL)
        self.assertTrue(url_on_stdin)
        # Any local user can read a process's argv, and the URL carries the camera's token.
        self.assertFalse(any("aBcToKeN123" in arg for arg in argv), argv)
        self.assertIn("--playlist=-", argv)

    def test_other_players_get_the_url_as_an_argument(self):
        argv, url_on_stdin = live.player_command(["vlc"], "Sauron", URL)
        self.assertFalse(url_on_stdin)
        self.assertEqual(argv, ["vlc", URL])

    def test_livestream_video_is_read_from_stdin(self):
        argv, url_on_stdin = live.player_command(["mpv"], "Sauron", None)
        self.assertFalse(url_on_stdin)
        self.assertEqual(argv, ["mpv", "--title=Sauron", "-"])


if __name__ == "__main__":
    unittest.main()
