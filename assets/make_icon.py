#!/usr/bin/env python3
"""Generate assets/icon.ico for the vpngate client (both binaries share it).

Design (v3.14,定稿 B · shield — see icon-candidates/DESIGN_NOTES.md §4):
rounded-square tile, SOLID indigo #1E40AF (no gradient), one single
white shield outline — a single element, no check mark, no keyhole,
no second element. The shield is a thick outline (stroke ≈ 0.11S,
2 px at 16 px) around an indigo core, top edge flat with chamfered
corners, straight sides, bottom tapering to a point. At 16x16 the
shield is still a complete closed frame with a visible point; at
256 px it is the same shape, just crisper. One element, one colour
+ white, no thin lines, no baked-in text (the V of v3.12–v3.13
violated the Microsoft icon guideline against letters in icons;
the v-bold rework is kept as a fallback in icon-candidates/v-bold/
if Leo ever wants the V back).

Sizes: 16/24/32/48/256 — each a NATIVE png (the 24/32/48/256 px
images are rendered from the vector master at 4x supersampling and
downscaled; the 16 px image is a hand-drawn pixel map, NOT a
downscale of the 256 px master — that downscale is exactly what
made the v3.13 tray icon unreadable). The .ico is packed by hand
(struct, no Pillow .ico writer) so each size keeps its own native
PNG entry; Pillow's .ico writer would resample everything from one
image.

Run:  python3 assets/make_icon.py
Out:   assets/icon.ico  (16/24/32/48/256) and assets/icon-256.png (preview),
       plus icon-preview-16/32/48.png next to this script's parent dir
       (the repo root) for a side-by-side check against the old icon
       (icon-preview-old-*.png are kept as the v3.13 reference).

The .ico is a generated binary: it is gitignored, NOT committed to the
repo (the snapshot push only takes text). The script is the source of
truth; build.rs embeds the .ico when it exists and warns otherwise.
"""

import os
import struct

from PIL import Image, ImageDraw

INDIGO = (30, 64, 175, 255)  # #1E40AF, same as the UI selection colour
WHITE = (255, 255, 255, 255)

# Shield master geometry, in 256 px coordinates (measured off
# icon-candidates/shield/icon-256.png, the design稿):
#   outer outline: flat top, chamfered top corners, straight sides,
#   diagonals meeting at the bottom point.
#   inner edge: the same shape inset by the 27 px stroke.
OUTER_256 = [
    (69, 34), (187, 34),  # flat top edge
    (194, 50),  # right top chamfer
    (194, 143),  # right side, down to where the diagonals start
    (128, 221),  # bottom point
    (62, 143),  # left side bottom
    (62, 50),  # left top chamfer
]
INNER_256 = [
    (94, 61), (162, 61),  # inner top edge (27 px below the outer top)
    (167, 80),  # right top chamfer
    (167, 134),  # right side
    (128, 194),  # inner bottom point
    (89, 134),  # left side
    (89, 80),  # left top chamfer
]


def _scale(points, size):
    f = size / 256.0
    return [(x * f, y * f) for (x, y) in points]


def draw_shield_vector(size):
    """Render the shield at `size` px from the vector master.

    Drawn at 4x and downscaled with LANCZOS (the candidate稿's
    "母稿超采样渲染"), so edges are antialiased rather than jagged.
    """
    ss = 4
    big = size * ss
    img = Image.new("RGBA", (big, big), (0, 0, 0, 0))
    tile = Image.new("RGBA", (big, big), INDIGO)
    mask = Image.new("L", (big, big), 0)
    ImageDraw.Draw(mask).rounded_rectangle(
        [0, 0, big - 1, big - 1], radius=int(round(0.21 * big)), fill=255
    )
    img.paste(tile, (0, 0), mask)
    d = ImageDraw.Draw(img)
    d.polygon(_scale(OUTER_256, big), fill=WHITE)
    d.polygon(_scale(INNER_256, big), fill=INDIGO)
    return img.resize((size, size), Image.LANCZOS)


# 16 px hand-drawn pixel map, copied verbatim from
# icon-candidates/shield/icon-16.png (the design稿's pixel稿):
# 'W' = white shield stroke, '.' = indigo tile/core, ' ' = transparent
# (rounded tile corners). The stroke is 2 px, the top bar is 2 rows,
# the sides are 2 px, the bottom point is 1 px — all hand-placed, not
# a downscale.
SHIELD_16 = [
    "  ............  ",
    " .............. ",
    "....WWWWWWWW....",
    "...WWWWWWWWWW...",
    "...WW......WW...",
    "...WW......WW...",
    "...WW......WW...",
    "...WW......WW...",
    "...WW......WW...",
    "...WWW....WWW...",
    "....WW....WW....",
    ".....WW..WW.....",
    "......WWWW......",
    ".......WW.......",
    " .............. ",
    "  ............  ",
]


def draw_shield_16():
    img = Image.new("RGBA", (16, 16), (0, 0, 0, 0))
    px = img.load()
    for y, row in enumerate(SHIELD_16):
        assert len(row) == 16, f"shield-16 row {y} is {len(row)} px, not 16"
        for x, ch in enumerate(row):
            if ch == "W":
                px[x, y] = WHITE
            elif ch == ".":
                px[x, y] = INDIGO
            elif ch == " ":
                pass
            else:  # pragma: no cover - guarded by the assert above
                raise ValueError(f"bad pixel {ch!r} at ({x}, {y})")
    return img


def draw_shield(size):
    if size == 16:
        return draw_shield_16()
    return draw_shield_vector(size)


def pack_ico(pngs):
    """Pack {size: png_bytes} into .ico bytes (PNG entries).

    Header: reserved(0) + type(1) + count; per entry: width byte
    (0 means 256), height byte, colour count 0, reserved 0, planes 1,
    bit count 32, byte size (u32 LE), offset (u32 LE); then the PNG
    payloads in size order.
    """
    sizes = sorted(pngs)
    header = struct.pack("<HHH", 0, 1, len(sizes))
    entries = b""
    offset = 6 + 16 * len(sizes)
    payloads = b""
    for s in sizes:
        data = pngs[s]
        w = 0 if s == 256 else s
        entries += struct.pack(
            "<BBBBHHII", w, w, 0, 0, 1, 32, len(data), offset
        )
        payloads += data
        offset += len(data)
    return header + entries + payloads


def main():
    here = os.path.dirname(os.path.abspath(__file__))
    root = os.path.dirname(here)
    images = {s: draw_shield(s) for s in (16, 24, 32, 48, 256)}
    # icon-256.png preview (the master render).
    images[256].save(os.path.join(here, "icon-256.png"))
    # .ico with one native PNG entry per size.
    import io

    pngs = {}
    for s, im in images.items():
        buf = io.BytesIO()
        im.save(buf, format="PNG")
        pngs[s] = buf.getvalue()
    with open(os.path.join(here, "icon.ico"), "wb") as f:
        f.write(pack_ico(pngs))
    # Downscaled previews are the ACTUAL per-size images (not a
    # downscale of the 256 px master), for the readability check.
    for s in (16, 32, 48):
        images[s].save(os.path.join(root, f"icon-preview-{s}.png"))
    print("wrote icon.ico (16/24/32/48/256 native PNGs), icon-256.png, icon-preview-16/32/48.png")


if __name__ == "__main__":
    main()
