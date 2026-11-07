#!/usr/bin/env python3
"""Generate assets/icon.ico for the vpngate client (both binaries share it).

Design: rounded-square tile, indigo -> cyan gradient, a white globe
(circle + meridian + equator) with a white swoosh arrow arcing over it
— traffic routed around the world. Flat, two colours + white, so it
still reads at 16x16.

Run:  python3 assets/make_icon.py
Out:   assets/icon.ico  (16/32/48/256) and assets/icon-256.png (preview)

The .ico is a generated binary: it is gitignored, NOT committed to the
repo (the snapshot push only takes text). The script is the source of
truth; build.rs embeds the .ico when it exists and warns otherwise.
"""

import math
from PIL import Image, ImageDraw

SIZE = 256
TOP = (30, 64, 175)      # indigo-700
BOTTOM = (8, 145, 178)   # cyan-700
WHITE = (255, 255, 255, 255)


def lerp(a, b, t):
    return tuple(int(a[i] + (b[i] - a[i]) * t) for i in range(3))


def base_tile():
    # Vertical gradient, then a rounded-square alpha mask.
    img = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
    grad = Image.new("RGB", (1, SIZE))
    for y in range(SIZE):
        grad.putpixel((0, y), lerp(TOP, BOTTOM, y / (SIZE - 1)))
    grad = grad.resize((SIZE, SIZE))
    mask = Image.new("L", (SIZE, SIZE), 0)
    ImageDraw.Draw(mask).rounded_rectangle(
        [0, 0, SIZE - 1, SIZE - 1], radius=54, fill=255
    )
    img.paste(grad, (0, 0), mask)
    return img


def draw_icon():
    img = base_tile()
    d = ImageDraw.Draw(img)
    cx, cy = 128, 128
    # Globe: circle + meridian + equator.
    r = 58
    d.ellipse([cx - r, cy - r, cx + r, cy + r], outline=WHITE, width=10)
    d.ellipse([cx - 26, cy - r, cx + 26, cy + r], outline=WHITE, width=7)
    d.line([cx - r, cy, cx + r, cy], fill=WHITE, width=7)
    # Swoosh arrow arcing over the top of the globe (200 -> 340 deg,
    # PIL arc angles run clockwise from 3 o'clock).
    orr = 86
    d.arc(
        [cx - orr, cy - orr, cx + orr, cy + orr],
        start=200, end=340, fill=WHITE, width=9,
    )
    # Round cap at the arc start.
    a0 = math.radians(200)
    sx, sy = cx + orr * math.cos(a0), cy + orr * math.sin(a0)
    d.ellipse([sx - 6, sy - 6, sx + 6, sy + 6], fill=WHITE)
    # Arrowhead at the arc end (340 deg), pointing along the tangent.
    a1 = math.radians(340)
    ex, ey = cx + orr * math.cos(a1), cy + orr * math.sin(a1)
    tx, ty = -math.sin(a1), math.cos(a1)  # tangent, direction of travel
    tip = (ex + tx * 13, ey + ty * 13)
    bc = (ex - tx * 9, ey - ty * 9)
    px, py = -ty, tx  # perpendicular
    c1 = (bc[0] + px * 8, bc[1] + py * 8)
    c2 = (bc[0] - px * 8, bc[1] - py * 8)
    d.polygon([tip, c1, c2], fill=WHITE)
    return img


def main():
    import os
    here = os.path.dirname(os.path.abspath(__file__))
    img = draw_icon()
    img.save(os.path.join(here, "icon-256.png"))
    img.save(
        os.path.join(here, "icon.ico"),
        sizes=[(16, 16), (32, 32), (48, 48), (256, 256)],
    )
    print("wrote icon.ico (16/32/48/256) and icon-256.png")


if __name__ == "__main__":
    main()
