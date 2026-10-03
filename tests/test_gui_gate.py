"""tests/gui_gate.py and tests/gui_fixture.py: the GUI gate's driver and its disc fixture."""

import os
import struct
import tempfile
import unittest
import zlib

import gui_fixture
import gui_gate


def bmp(width, height, pixels):
    """A 32-bit bottom-up BMP of `pixels` (rows top-down, (r, g, b) each)."""
    data = b"".join(bytes([b, g, r, 0]) for row in reversed(pixels) for (r, g, b) in row)
    info = struct.pack("<IiiHHIIiiII", 40, width, height, 1, 32, 0, len(data), 0, 0, 0, 0)
    head = b"BM" + struct.pack("<IHHI", 14 + 40 + len(data), 0, 0, 14 + 40)
    return head + info + data


def png_rows(png):
    """The unfiltered RGB rows of a PNG written by `bmp_to_png`."""
    assert png.startswith(b"\x89PNG\r\n\x1a\n")
    pos, idat, width = 8, b"", 0
    while pos < len(png):
        n = struct.unpack_from(">I", png, pos)[0]
        kind, data = png[pos + 4:pos + 8], png[pos + 8:pos + 8 + n]
        if kind == b"IHDR":
            width = struct.unpack_from(">I", data)[0]
        if kind == b"IDAT":
            idat += data
        pos += 12 + n
    raw, line = zlib.decompress(idat), 1 + width * 3
    return [raw[i + 1:i + line] for i in range(0, len(raw), line)]


class BmpToPng(unittest.TestCase):
    def test_pixels_survive_top_down_and_rgb(self):
        red, blue = (255, 0, 0), (0, 0, 255)
        rows = png_rows(gui_gate.bmp_to_png(bmp(2, 2, [[red, blue], [blue, red]])))
        self.assertEqual(rows, [bytes([255, 0, 0, 0, 0, 255]), bytes([0, 0, 255, 255, 0, 0])])

    def test_only_32_bit_is_accepted(self):
        b = bytearray(bmp(1, 1, [[(1, 2, 3)]]))
        b[28] = 24
        with self.assertRaises(ValueError):
            gui_gate.bmp_to_png(bytes(b))


class RunEnv(unittest.TestCase):
    def test_every_run_opens_the_fixture_into_the_gate_with_no_drive_probe(self):
        env = gui_gate.run_env("de", 144, "/fx", "/out", "/profile")
        self.assertEqual((env["FMKV_OPEN"], env["FMKV_GATE"], env["FMKV_NO_PROBE"]), ("/fx", "/out", "1"))
        if os.name == "nt":
            self.assertEqual((env["FMKV_SYSTEM_LOCALE"], env["FMKV_DPI"], env["APPDATA"]), ("de", "144", "/profile"))
        else:
            self.assertEqual((env["LANGUAGE"], env["GDK_DPI_SCALE"], env["XDG_DATA_HOME"]), ("de", "1.5", "/profile"))

    def test_the_matrix_is_three_languages_at_two_scales(self):
        self.assertEqual(gui_gate.LANGS, ["en", "de", "ar"])
        self.assertEqual([d * 100 // 96 for d in gui_gate.DPIS], [100, 150])


class Fixture(unittest.TestCase):
    def test_the_folder_is_a_small_unencrypted_blu_ray(self):
        with tempfile.TemporaryDirectory() as d:
            gui_fixture.main(d)
            names = sorted(os.path.relpath(os.path.join(r, f), d).replace(os.sep, "/")
                           for r, _, fs in os.walk(d) for f in fs)
            self.assertEqual(names, ["BDMV/CLIPINF/00000.clpi", "BDMV/PLAYLIST/00000.mpls",
                                     "BDMV/PLAYLIST/00001.mpls", "BDMV/STREAM/00000.m2ts",
                                     "BDMV/index.bdmv"])
            m2ts = os.path.getsize(os.path.join(d, "BDMV/STREAM/00000.m2ts"))
            self.assertEqual(m2ts, gui_fixture.PACKETS * 192)
            with open(os.path.join(d, "BDMV/PLAYLIST/00000.mpls"), "rb") as f:
                mpls = f.read()
            self.assertTrue(mpls.startswith(b"MPLS0200"))
            # The PlayItem's out time: 2:17:00 past the 1 s in time, in 45 kHz ticks.
            self.assertIn(struct.pack(">I", 45_000 * (1 + 2 * 3600 + 17 * 60)), mpls)


if __name__ == "__main__":
    unittest.main()
