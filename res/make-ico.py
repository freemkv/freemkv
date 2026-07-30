#!/usr/bin/env python3
"""Regenerate res/freemkv.ico from the two checked-in SVG sources.

    pip install pillow cairosvg      # or: brew install librsvg
    python3 res/make-ico.py          # writes res/freemkv.ico
    python3 res/make-ico.py --sheet  # also writes a magnified contact sheet

Why two sources? Windows picks a frame out of the .ico per context (16 px is
the title bar, 32 px Alt-Tab and the taskbar, 256 px Explorer's large view),
and the full-detail mark does not survive downscaling below 32 px -- thin
rings and the play-arrow glyph turn to mush. So sizes <= 24 px are rendered
from the simplified `freemkv-icon-small.svg` and sizes >= 32 px from the
full-detail `freemkv-icon.svg`. See the comment at the top of the small SVG.

Keep this script in step with build.rs, which reads the .ico at build time and
synthesises the RT_ICON / RT_GROUP_ICON resources itself -- so the .ico here is
the single source of truth for the Windows executable icon.
"""

from __future__ import annotations

import argparse
import io
import shutil
import subprocess
import sys
from pathlib import Path

RES = Path(__file__).resolve().parent
FULL = RES / "freemkv-icon.svg"
SMALL = RES / "freemkv-icon-small.svg"
OUT = RES / "freemkv.ico"

# Windows asks for these. 16/20/24 cover the title bar and the 125%/150% DPI
# small-icon metrics; 32/40/48 Alt-Tab, taskbar and Explorer medium; 64/128/256
# Explorer large and extra-large.
SIZES = [16, 20, 24, 32, 40, 48, 64, 128, 256]
SIMPLIFY_AT_OR_BELOW = 24


def rasterize(svg: Path, size: int) -> bytes:
    """Render `svg` to a `size`x`size` RGBA PNG, returning the PNG bytes."""
    if shutil.which("rsvg-convert"):
        return subprocess.run(
            ["rsvg-convert", "-w", str(size), "-h", str(size), str(svg)],
            check=True,
            capture_output=True,
        ).stdout
    try:
        import cairosvg
    except ImportError:
        sys.exit(
            "need an SVG rasterizer: `pip install cairosvg` or "
            "`brew install librsvg` (for rsvg-convert)"
        )
    return cairosvg.svg2png(url=str(svg), output_width=size, output_height=size)


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument(
        "--sheet",
        action="store_true",
        help="also write make-ico-sheet.png, a nearest-neighbour magnified "
        "contact sheet for eyeballing small-size legibility",
    )
    args = ap.parse_args()

    try:
        from PIL import Image
    except ImportError:
        sys.exit("need Pillow to write the .ico: `pip install pillow`")

    frames = []
    for size in SIZES:
        src = SMALL if size <= SIMPLIFY_AT_OR_BELOW else FULL
        png = rasterize(src, size)
        frames.append((size, Image.open(io.BytesIO(png)).convert("RGBA")))
        print(f"  {size:>3} px  <- {src.name}")

    # Pillow matches each requested size against the images it is given and
    # resamples only when none matches — so EVERY rendered frame has to be
    # handed over through `append_images`. Passing only the 256 px image (an
    # easy mistake, since it still yields a valid multi-resolution .ico) makes
    # every small frame a LANCZOS downscale of the full-detail art, which is
    # precisely the mush the small SVG exists to avoid. The base image must be
    # the largest, because Pillow skips any requested size bigger than it.
    base = frames[-1][1]
    base.save(
        OUT,
        format="ICO",
        sizes=[(s, s) for s, _ in frames],
        append_images=[im for _, im in frames[:-1]],
    )
    print(f"wrote {OUT.relative_to(RES.parent)} ({OUT.stat().st_size} bytes)")

    if args.sheet:
        cell, pad = 160, 12
        small = [f for f in frames if f[0] <= 48]
        sheet = Image.new(
            "RGB",
            (len(small) * (cell + pad) + pad, cell + pad * 2),
            (235, 235, 235),
        )
        x = pad
        for _, im in small:
            flat = Image.new("RGBA", im.size, (255, 255, 255, 255))
            flat.alpha_composite(im)
            sheet.paste(flat.convert("RGB").resize((cell, cell), Image.NEAREST), (x, pad))
            x += cell + pad
        sheet.save(RES / "make-ico-sheet.png")
        print(f"wrote {(RES / 'make-ico-sheet.png').relative_to(RES.parent)}")


if __name__ == "__main__":
    main()
