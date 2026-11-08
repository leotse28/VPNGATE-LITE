#!/usr/bin/env python3
"""Generate assets/icon.ico for the vpngate client (both binaries share it).

Design (v3.13, redrawn — the old globe + swoosh was unreadable at 16 px):
rounded-square tile, SOLID indigo #1E40AF (no gradient), one single
bold white "V" — thick strokes, big margins, nothing else. At 16x16
the V is still two thick arms and a point; at 256 px it is the same
shape, just crisper. One element, one colour + white, no thin lines.

Run:  python3 assets/make_icon.py
Out:   assets/icon.ico  (16/32/48/256) and assets/icon-256.png (preview),
       plus icon-preview-16/32/48.png next to this script's parent dir
       (the repo root) for a side-by-side check against the old icon.

The .ico is a generated binary: it is gitignored, NOT committed to the
repo (the snapshot push only takes text). The script is the source of
truth; build.rs embeds the .ico when it exists and warns otherwise.
"""

from PIL import Image, ImageDraw

SIZE = 256
INDIGO = (30, 64, 175, 255)  # #1E40AF, same as the UI selection colour
WHITE = (255, 255, 255, 255)


def draw_icon():
    # Solid indigo tile with a rounded-square alpha mask.
    img = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
    tile = Image.new("RGBA", (SIZE, SIZE), INDIGO)
    mask = Image.new("L", (SIZE, SIZE), 0)
    ImageDraw.Draw(mask).rounded_rectangle(
        [0, 0, SIZE - 1, SIZE - 1], radius=54, fill=255
    )
    img.paste(tile, (0, 0), mask)
    d = ImageDraw.Draw(img)
    # Bold "V": two thick arms meeting at the bottom centre, with
    # generous margins (the strokes stay clear of the tile edge by
    # ~50 px at 256 px, i.e. ~3 px at 16 px). Stroke width 40 px at
    # 256 px (~2.5 px at 16 px) — thick enough to survive downscaling.
    # Round caps (circles at both ends + the vertex) so the ends do
    # not look clipped at small sizes.
    left = (62, 66)
    vertex = (128, 196)
    right = (194, 66)
    width = 40
    d.line([left, vertex], fill=WHITE, width=width, joint="curve")
    d.line([vertex, right], fill=WHITE, width=width, joint="curve")
    r = width // 2
    for cx, cy in (left, vertex, right):
        d.ellipse([cx - r, cy - r, cx + r, cy + r], fill=WHITE)
    return img


def main():
    import os
    here = os.path.dirname(os.path.abspath(__file__))
    root = os.path.dirname(here)
    img = draw_icon()
    img.save(os.path.join(here, "icon-256.png"))
    img.save(
        os.path.join(here, "icon.ico"),
        sizes=[(16, 16), (32, 32), (48, 48), (256, 256)],
    )
    # Downscaled previews for the 16/32/48 px readability check.
    for s in (16, 32, 48):
        img.resize((s, s), Image.LANCZOS).save(
            os.path.join(root, f"icon-preview-{s}.png")
        )
    print("wrote icon.ico (16/32/48/256), icon-256.png, icon-preview-16/32/48.png")


if __name__ == "__main__":
    main()
