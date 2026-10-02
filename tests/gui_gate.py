#!/usr/bin/env python3
"""qa's GUI gate driver: run a debug build of the desktop app over the synthetic disc folder
(tests/gui_fixture.py) in English, German and Arabic at 100% and 150%, once per pair.

    python3 tests/gui_gate.py APP OUT_DIR

APP is the debug app binary: `freemkv-gui.exe` on Windows, `freemkv` built with
`--features gui` on Linux (run it under `xvfb-run` and `dbus-run-session`). Each run sets
FMKV_GATE, so the app checks its own title-tree columns and Settings fields and dropdowns
(`src/gui_gate.rs`, `src/windows.rs`, `src/linux/gate.rs`), writes `gate.txt` and its
captures to OUT_DIR/<lang>-<scale>/, and exits non-zero on a failed check. This exits 1 if any
run failed, crashed, timed out or wrote no report. Windows captures are BMP and become PNG here.
"""

import os
import struct
import subprocess
import sys
import zlib

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import gui_fixture  # noqa: E402

LANGS = ["en", "de", "ar"]
DPIS = [96, 144]
TIMEOUT_S = 180


def run_env(lang, dpi, fixture, out, profile):
    """The environment one run gets: the fixture, the gate, a fresh profile, the language
    "Auto" resolves to and the scale."""
    env = dict(os.environ, FMKV_OPEN=fixture, FMKV_GATE=out, FMKV_NO_PROBE="1")
    if os.name == "nt":
        env.update(APPDATA=profile, FMKV_SYSTEM_LOCALE=lang, FMKV_DPI=str(dpi))
    else:
        env.update(XDG_DATA_HOME=profile, XDG_CONFIG_HOME=profile, XDG_CACHE_HOME=profile,
                   LANGUAGE=lang, LANG="C.UTF-8", GDK_DPI_SCALE=str(dpi / 96),
                   GSK_RENDERER="cairo")
    return env


def bmp_to_png(bmp):
    """A 32-bit bottom-up BMP (what the Windows app writes) as PNG bytes, alpha dropped."""
    offset = struct.unpack_from("<I", bmp, 10)[0]
    width, height = struct.unpack_from("<ii", bmp, 18)
    bpp = struct.unpack_from("<H", bmp, 28)[0]
    if bpp != 32:
        raise ValueError(f"{bpp}-bit BMP")
    stride, rows = width * 4, []
    for y in range(abs(height)):
        src = (abs(height) - 1 - y) if height > 0 else y
        line = bmp[offset + src * stride: offset + (src + 1) * stride]
        rgb = bytearray(width * 3)
        rgb[0::3], rgb[1::3], rgb[2::3] = line[2::4], line[1::4], line[0::4]
        rows.append(b"\x00" + bytes(rgb))

    def chunk(kind, data):
        body = kind + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body))

    header = struct.pack(">IIBBBBB", width, abs(height), 8, 2, 0, 0, 0)
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", header)
            + chunk(b"IDAT", zlib.compress(b"".join(rows), 9)) + chunk(b"IEND", b""))


def pngify(directory):
    for name in sorted(os.listdir(directory)):
        if name.endswith(".bmp"):
            path = os.path.join(directory, name)
            with open(path, "rb") as f:
                png = bmp_to_png(f.read())
            with open(path[:-4] + ".png", "wb") as f:
                f.write(png)
            os.remove(path)


def main(app, root):
    app, root = os.path.abspath(app), os.path.abspath(root)
    fixture = os.path.join(root, "fixture")
    gui_fixture.main(fixture)
    failed = []
    for lang in LANGS:
        for dpi in DPIS:
            tag = f"{lang}-{dpi * 100 // 96}"
            out = os.path.join(root, tag)
            profile = os.path.join(root, "profiles", tag)
            os.makedirs(out, exist_ok=True)
            os.makedirs(profile, exist_ok=True)
            print(f"── {tag} ──", flush=True)
            try:
                rc = subprocess.run([app], env=run_env(lang, dpi, fixture, out, profile),
                                    timeout=TIMEOUT_S).returncode
            except subprocess.TimeoutExpired:
                rc = "timeout"
            report = os.path.join(out, "gate.txt")
            if os.path.exists(report):
                with open(report, encoding="utf-8") as f:
                    print(f.read(), end="")
            else:
                print("no gate report: the app crashed or never reached the gate")
            pngify(out)
            if rc != 0 or not os.path.exists(report):
                failed.append(f"{tag} (exit {rc})")
    if failed:
        print(f"::error title=GUI gate failed::{', '.join(failed)}")
        return 1
    print(f"GUI gate: {len(LANGS) * len(DPIS)} runs passed")
    return 0


if __name__ == "__main__":
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    sys.exit(main(sys.argv[1], sys.argv[2]))
