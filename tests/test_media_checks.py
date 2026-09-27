import copy
import json
from pathlib import Path
import tempfile
import media_kat as kat
import subprocess
import unittest
from unittest.mock import patch

import media_checks as checks


def fixture():
    return {"format": {"duration": "2.0"},
            "streams": [{"index": 0, "codec_type": "video"},
                        {"index": 1, "codec_type": "audio"}],
            "packets": [{"stream_index": i, "pts_time": str(n * 0.04), "duration_time": "0.04"}
                        for i in range(2) for n in range(50)]}


class TimelineTests(unittest.TestCase):
    def test_valid_timeline(self):
        self.assertEqual(len(checks.timeline(fixture())), 2)

    def test_no_packets_fails(self):
        data = fixture()
        data["packets"] = []
        with self.assertRaises(checks.ValidationError):
            checks.timeline(data)

    def test_missing_audio_packets_fails(self):
        data = fixture()
        data["packets"] = data["packets"][:50]
        with self.assertRaises(checks.ValidationError):
            checks.timeline(data)

    def test_unknown_stream_fails(self):
        data = fixture()
        data["packets"][0]["stream_index"] = 9
        with self.assertRaises(checks.ValidationError):
            checks.timeline(data)

    def test_invalid_time_fails(self):
        for value in ["nan", "inf", "garbage"]:
            with self.subTest(value=value):
                data = fixture()
                data["packets"][0]["pts_time"] = value
                with self.assertRaises(checks.ValidationError):
                    checks.timeline(data)

    def test_no_timestamped_packets_fails(self):
        data = fixture()
        for packet in data["packets"]:
            packet.pop("pts_time")
        with self.assertRaises(checks.ValidationError):
            checks.timeline(data)

    def test_collapsed_audio_fails_even_in_cadence_mode(self):
        data = fixture()
        for packet in data["packets"][50:]:
            packet["pts_time"] = "0"
        with self.assertRaises(checks.ValidationError):
            checks.timeline(data, "cadence")

    def test_cadence_allows_legitimate_late_start(self):
        data = fixture()
        data["format"]["duration"] = "4"
        for packet in data["packets"]:
            packet["pts_time"] = str(float(packet["pts_time"]) + 2)
        checks.timeline(data, "cadence")
        with self.assertRaises(checks.ValidationError):
            checks.timeline(data, "strict")

    def test_cadence_does_not_hide_backward_jumps(self):
        data = fixture()
        data["packets"][90]["pts_time"] = "0"
        with self.assertRaises(checks.ValidationError):
            checks.timeline(data, "cadence")

    def test_bframe_presentation_reordering_is_allowed(self):
        data = fixture()
        data["packets"][1], data["packets"][2] = data["packets"][2], data["packets"][1]
        checks.timeline(data)

    def test_sparse_subtitles_need_not_span_movie(self):
        data = fixture()
        data["streams"].append({"index": 2, "codec_type": "subtitle"})
        data["packets"].append({"stream_index": 2, "pts_time": "1"})
        checks.timeline(data)
        data["packets"][-1]["pts_time"] = "10"
        with self.assertRaises(checks.ValidationError):
            checks.timeline(data)


    def test_missing_individual_pts_fails(self):
        data = fixture()
        del data["packets"][70]["pts_time"]
        with self.assertRaises(checks.ValidationError):
            checks.timeline(data, "cadence")

    def test_vc1_vfw_block_timestamps_surface_as_dts(self):
        # Matroska V_MS/VFW/FOURCC (VC-1, HD-DVD): ffprobe reports the block
        # timestamp as DTS and leaves pts N/A on anchors (measured on a real rip).
        data = fixture()
        data["streams"][0].update(codec_name="vc1", codec_tag_string="WVC1")
        for n, packet in enumerate(data["packets"][:50]):
            packet["dts_time"] = packet["pts_time"]
            if n % 3 != 1:
                del packet["pts_time"]
        checks.timeline(data, "cadence")
        del data["packets"][3]["dts_time"]
        with self.assertRaisesRegex(checks.ValidationError, "missing packet timestamp"):
            checks.timeline(data, "cadence")

    def test_dts_does_not_stand_in_for_pts_outside_vc1(self):
        data = fixture()
        data["streams"][0]["codec_name"] = "h264"
        data["packets"][3]["dts_time"] = data["packets"][3].pop("pts_time")
        with self.assertRaisesRegex(checks.ValidationError, "missing packet timestamp"):
            checks.timeline(data, "cadence")

    def test_uniformly_compressed_audio_fails(self):
        data = fixture()
        for packet in data["packets"][50:]:
            packet["pts_time"] = str(float(packet["pts_time"]) / 100)
        with self.assertRaises(checks.ValidationError):
            checks.timeline(data, "cadence")

    def test_large_negative_shift_fails(self):
        data = fixture()
        for packet in data["packets"]:
            packet["pts_time"] = str(float(packet["pts_time"]) - 100)
        with self.assertRaises(checks.ValidationError):
            checks.timeline(data)


class KnownAnswerTests(unittest.TestCase):
    def test_content_metadata_and_timing_mutations_fail(self):
        expected = json.loads((kat.FIXTURES / "answers.json").read_text())["cases"]["cfr"]["streams"]
        self.assertTrue(kat.matches_streams(expected, expected))
        for field, value in [("sha256", "bad"), ("frames", 0), ("samples", 1),
                             ("language", "fra"), ("format", {}), ("bytes", 0)]:
            actual = copy.deepcopy(expected)
            actual[0][field] = value
            with self.subTest(field=field):
                self.assertFalse(kat.matches_streams(actual, expected))
        self.assertFalse(kat.matches_streams(expected[:-1], expected))
        actual = copy.deepcopy(expected)
        actual[0]["pts_us"][0] += 2000
        self.assertFalse(kat.matches_streams(actual, expected))

    def test_padding_moved_to_the_wrong_block_fails(self):
        expected = json.loads((kat.FIXTURES / "answers.json").read_text())["cases"]["cfr"]["timing"]
        actual = copy.deepcopy(expected)
        self.assertTrue(actual["padding"])
        actual["padding"][0]["pts_us"] = 0
        self.assertFalse(kat.matches_timing(actual, expected))
        actual = copy.deepcopy(expected)
        actual["tracks"][1]["delay_ns"] = 0
        self.assertFalse(kat.matches_timing(actual, expected))

    def test_parity_requires_all_platforms_and_cases(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            with self.assertRaises(OSError):
                kat.compare_reports(root)
            for platform in ("ubuntu-latest", "macos-latest", "windows-latest"):
                folder = root / f"media-{platform}"
                folder.mkdir()
                (folder / "answers.json").write_text("{}")
            with self.assertRaises(checks.ValidationError):
                kat.compare_reports(root)

    def test_parity_accepts_identical_and_rejects_divergent_platforms(self):
        report = {f"{name}-{n}": {"timing": {"duration_ns": 2_000_000_000}}
                  for name in ("cfr", "vfr", "pgs") for n in range(2)}
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            for platform in ("ubuntu-latest", "macos-latest", "windows-latest"):
                (root / f"media-{platform}").mkdir()
                (root / f"media-{platform}" / "answers.json").write_text(json.dumps(report))
            with patch("builtins.print"):
                kat.compare_reports(root)
            divergent = copy.deepcopy(report)
            divergent["vfr-1"]["timing"]["duration_ns"] += 1
            (root / "media-windows-latest" / "answers.json").write_text(json.dumps(divergent))
            with self.assertRaisesRegex(checks.ValidationError, "differs across platforms"):
                kat.compare_reports(root)


class ToolTests(unittest.TestCase):
    @patch("media_checks.subprocess.run")
    def test_silent_process_failure_is_fatal(self, process):
        process.return_value = subprocess.CompletedProcess([], 7, b"", b"")
        with self.assertRaises(checks.ValidationError):
            checks.decode("unused", [0])
        with self.assertRaises(checks.ValidationError):
            checks.probe("unused", packets=True)

    @patch("media_checks.probe", return_value={"streams": [{"codec_type": "video"}]})
    @patch("media_checks.subprocess.run")
    def test_zero_exit_with_no_decoded_frames_is_fatal(self, process, probe):
        process.return_value = subprocess.CompletedProcess([], 0, b"# header\n", b"")
        with self.assertRaises(checks.ValidationError):
            checks.decode("unused", [0])

    @patch("media_checks.subprocess.run")
    def test_probe_rejects_invalid_or_empty_json(self, process):
        for value in [b"", b"invalid", b"{}", b"[]", b'{"streams":[]}']:
            process.return_value = subprocess.CompletedProcess([], 0, value, b"")
            with self.subTest(value=value), self.assertRaises(checks.ValidationError):
                checks.probe("unused")

    @patch("media_checks.probe", return_value={"streams": [{"codec_type": "video"}]})
    @patch("media_checks.subprocess.run")
    def test_decoder_diagnostics_are_not_suppressed(self, process, probe):
        process.return_value = subprocess.CompletedProcess([], 0, b"0,1,2,3\n", b"corrupt frame")
        with self.assertRaises(checks.ValidationError):
            checks.decode("unused", [0])

    @patch("media_checks.probe", return_value={"streams": [{"codec_type": "video"}, {"codec_type": "audio"}]})
    @patch("media_checks.subprocess.run")
    def test_video_cannot_hide_empty_audio(self, process, probe):
        process.return_value = subprocess.CompletedProcess([], 0, b"0,0,0,1,100,hash\n", b"")
        with self.assertRaises(checks.ValidationError):
            checks.decode("unused", [0])


if __name__ == "__main__":
    unittest.main()
