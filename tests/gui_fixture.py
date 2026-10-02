#!/usr/bin/env python3
"""Write the GUI gate's disc fixture: a small, synthetic, unencrypted Blu-ray folder.

    python3 tests/gui_fixture.py OUT_DIR

OUT_DIR gets BDMV/{index.bdmv, PLAYLIST, CLIPINF, STREAM}: two playlists (a 2:17:00 main
title and a 12:30 extra) over one LPCM clip of real TS/PES packets, so the app's title tree
shows two titles with a Length and a Size. No disc data: every byte is generated here, the
same layout as `src/ku_fixture.rs` (after freemkv-engine's `test_fixtures`), without AACS.
"""

import os
import struct
import sys

AUDIO_PID = 0x1100
# Source packets in the clip: 6 MB of stream, so the Size column reads "6 MB".
PACKETS = 32_768
# (playlist, running time in seconds)
TITLES = [("00000", 2 * 3600 + 17 * 60), ("00001", 12 * 60 + 30)]


def mpls(clip, secs):
    """A one-PlayItem MPLS on `clip` whose STN lists the LPCM stream; in/out in 45 kHz ticks."""
    item = clip.encode() + b"M2TS" + bytes(3)
    item += struct.pack(">II", 45_000, 45_000 * (1 + secs)) + bytes(12)
    stn = bytes([0, 0, 0, 0, 0, 1]) + bytes(10)
    stn += bytes([3, 0x01]) + struct.pack(">H", AUDIO_PID)
    stn += bytes([5, 0x80, 0x31]) + b"eng"
    item += stn
    pl = bytes(6) + struct.pack(">H", 1) + bytes(2) + struct.pack(">H", len(item)) + item
    pl = struct.pack(">I", len(pl) - 4) + pl[4:]
    return b"MPLS0200" + struct.pack(">I", 40) + bytes(28) + pl


def clpi(source_packets):
    """A CLPI with what `clpi::parse` needs: magic and the source packet count at 56."""
    d = bytearray(60)
    d[0:8] = b"HDMV0200"
    d[56:60] = struct.pack(">I", source_packets)
    return bytes(d)


def source_packet(k, n, secs):
    """Source packet `k`: TP_extra_header and one TS packet holding one whole LPCM PES."""
    audio = 160
    p = bytearray(192)
    p[0:4] = struct.pack(">I", (k * 100) & 0x3FFF_FFFF)
    ts = bytearray(188)
    ts[0:4] = bytes([0x47, 0x40 | (AUDIO_PID >> 8), AUDIO_PID & 0xFF, 0x30 | (k & 0x0F)])
    stuffing = 188 - 4 - 1 - (14 + 4 + audio)
    ts[4] = stuffing
    ts[5] = 0x00
    ts[6:5 + stuffing] = b"\xff" * (stuffing - 1)
    pts = 90_000 + k * 90_000 * secs // n
    pes = bytearray(14 + 4 + audio)
    pes[0:4] = b"\x00\x00\x01\xbd"
    pes[4:6] = struct.pack(">H", 8 + 4 + audio)
    pes[6:9] = bytes([0x81, 0x80, 5])
    pes[9] = 0x21 | ((pts >> 29) & 0x0E)
    pes[10:12] = struct.pack(">H", ((pts >> 14) & 0xFFFE) | 1)
    pes[12:14] = struct.pack(">H", ((pts << 1) & 0xFFFE) | 1)
    pes[14:16] = struct.pack(">H", audio)
    pes[16] = 0x31
    pes[17] = 0x40
    pes[18:] = bytes((k * 7 + i) & 0xFF for i in range(audio))
    ts[5 + stuffing:] = pes
    p[4:] = ts
    return bytes(p)


def main(out):
    def put(rel, data):
        path = os.path.join(out, *rel.split("/"))
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "wb") as f:
            f.write(data)

    put("BDMV/index.bdmv", bytes(2048))
    for name, secs in TITLES:
        put(f"BDMV/PLAYLIST/{name}.mpls", mpls("00000", secs))
    put("BDMV/CLIPINF/00000.clpi", clpi(PACKETS))
    main_secs = TITLES[0][1]
    put("BDMV/STREAM/00000.m2ts",
        b"".join(source_packet(k, PACKETS, main_secs) for k in range(PACKETS)))
    print(out)


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    main(sys.argv[1])
