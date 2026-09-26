#!/usr/bin/env python3
"""Generate AnimeHub's icon source (1024x1024 RGBA PNG).

Deliberately dependency-light: Pillow only. The result is fed to
`tauri icon` (see scripts/build.sh), which derives every platform format.

Design: a rounded-square "hub" tile whose negative space is a play triangle,
with three orbiting dots for the sites it aggregates.
"""

from __future__ import annotations

import math
import sys
from pathlib import Path

from PIL import Image, ImageDraw

SIZE = 1024
OUT = Path(__file__).resolve().parent.parent / "src-tauri" / "icon.png"

# Indigo -> rose, matching the launcher's accent ramp.
TOP = (99, 102, 241)      # indigo-500
BOTTOM = (225, 29, 72)    # rose-600


def vertical_gradient(size: int) -> Image.Image:
    """Linear top-to-bottom gradient."""
    img = Image.new("RGBA", (size, size))
    px = img.load()
    for y in range(size):
        t = y / (size - 1)
        r = round(TOP[0] + (BOTTOM[0] - TOP[0]) * t)
        g = round(TOP[1] + (BOTTOM[1] - TOP[1]) * t)
        b = round(TOP[2] + (BOTTOM[2] - TOP[2]) * t)
        for x in range(size):
            px[x, y] = (r, g, b, 255)
    return img


def rounded_mask(size: int, radius: int) -> Image.Image:
    """Squircle-ish mask with 4x supersampling for smooth edges."""
    ss = 4
    big = Image.new("L", (size * ss, size * ss), 0)
    d = ImageDraw.Draw(big)
    d.rounded_rectangle(
        [0, 0, size * ss - 1, size * ss - 1], radius=radius * ss, fill=255
    )
    return big.resize((size, size), Image.LANCZOS)


def play_triangle(size: int) -> Image.Image:
    """A play triangle cut out of the tile."""
    ss = 4
    big = Image.new("L", (size * ss, size * ss), 0)
    d = ImageDraw.Draw(big)
    cx, cy = size * ss / 2, size * ss / 2
    w, h = size * ss * 0.34, size * ss * 0.40
    pts = [
        (cx - w / 2 + w * 0.06, cy - h / 2),
        (cx - w / 2 + w * 0.06, cy + h / 2),
        (cx + w / 2 + w * 0.06, cy),
    ]
    d.polygon(pts, fill=255)
    return big.resize((size, size), Image.LANCZOS)


def orbit_dots(size: int) -> Image.Image:
    """Three dots orbiting the triangle: the aggregated sites."""
    ss = 4
    big = Image.new("L", (size * ss, size * ss), 0)
    d = ImageDraw.Draw(big)
    cx, cy = size * ss / 2, size * ss / 2
    radius = size * ss * 0.40
    dot = size * ss * 0.045
    for i, angle_deg in enumerate((200, 320, 80)):
        a = math.radians(angle_deg)
        x = cx + radius * math.cos(a)
        y = cy + radius * math.sin(a)
        # The leading dot is largest, reading as "now playing".
        r = dot * (1.25 if i == 2 else 1.0)
        d.ellipse([x - r, y - r, x + r, y + r], fill=255)
    return big.resize((size, size), Image.LANCZOS)


def main() -> int:
    tile = vertical_gradient(SIZE)
    mask = rounded_mask(SIZE, int(SIZE * 0.22))

    icon = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
    icon.paste(tile, (0, 0), mask)

    # Knock the triangle and dots out to a translucent white so the gradient
    # still reads through the glyph.
    overlay = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
    od = ImageDraw.Draw(overlay)
    glyph = Image.composite(
        Image.new("RGBA", (SIZE, SIZE), (255, 255, 255, 235)),
        Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0)),
        play_triangle(SIZE),
    )
    overlay = Image.alpha_composite(overlay, glyph)
    dots = Image.composite(
        Image.new("RGBA", (SIZE, SIZE), (255, 255, 255, 200)),
        Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0)),
        orbit_dots(SIZE),
    )
    overlay = Image.alpha_composite(overlay, dots)

    icon = Image.alpha_composite(icon, overlay)

    OUT.parent.mkdir(parents=True, exist_ok=True)
    icon.save(OUT, "PNG")
    print(f"wrote {OUT} ({icon.size[0]}x{icon.size[1]})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
