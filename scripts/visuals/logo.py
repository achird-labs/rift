"""Derives the logo PNGs in docs/assets from the owner's source image.

    python3 scripts/visuals/logo.py "<path to Rift Gradient Tech Logo.png>"

Needs Pillow, so it is not part of check.sh (which is stdlib only); run it by hand when the source
changes. The source is a gradient "R" mark above a white "rift" wordmark on a flat navy background.

Transparency is an alpha matte: a pixel's alpha grows with its colour distance from the background,
and its colour is un-mixed from the background, so the antialiased edges stay smooth on any page.

Writes:
  logo-dark.png  mark + wordmark, for dark backgrounds (white wordmark), ~480 px wide
  logo.png       mark + wordmark, for light backgrounds (navy wordmark)
  logo-mark.png  mark only, square, 256 px
  favicon.png    mark only, square, 64 px
"""

import os
import sys

from PIL import Image

OUT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "docs", "assets")
NAVY = (14, 15, 29)  # #0e0f1d, the source background; the light variant's wordmark colour
LO, HI = 10.0, 110.0  # distance from the background: <= LO is fully clear, >= HI fully opaque


def background(im: Image.Image) -> tuple[int, int, int]:
    """The median colour of the image border."""
    w, h = im.size
    px = im.load()
    border = [px[x, y] for x in range(w) for y in (0, h - 1)] + [px[x, y] for y in range(h) for x in (0, w - 1)]
    return tuple(sorted(c[i] for c in border)[len(border) // 2] for i in range(3))


def matte(im: Image.Image, bg: tuple[int, int, int]) -> Image.Image:
    """RGBA with alpha from the distance to `bg`, and colour un-mixed from it."""
    src = im.load()
    out = Image.new("RGBA", im.size)
    dst = out.load()
    for y in range(im.size[1]):
        for x in range(im.size[0]):
            c = src[x, y]
            d = sum((a - b) ** 2 for a, b in zip(c, bg)) ** 0.5
            a = min(1.0, max(0.0, (d - LO) / (HI - LO)))
            if a == 0.0:
                dst[x, y] = (0, 0, 0, 0)
                continue
            rgb = tuple(max(0, min(255, round(b + (v - b) / a))) for v, b in zip(c, bg))
            dst[x, y] = (*rgb, round(a * 255))
    return out


def split_rows(rgba: Image.Image) -> int:
    """The middle of the widest empty band of rows: the gap between the mark and the wordmark."""
    alpha = rgba.getchannel("A")
    w, h = rgba.size
    rows = [y for y in range(h) if alpha.crop((0, y, w, y + 1)).getbbox()]
    gap = max(zip(rows, rows[1:]), key=lambda p: p[1] - p[0])
    return (gap[0] + gap[1]) // 2


def resize(im: Image.Image, size: tuple[int, int]) -> Image.Image:
    # Premultiplied, so clear pixels' colours cannot bleed into the edges.
    return im.convert("RGBa").resize(size, Image.LANCZOS).convert("RGBA")


def trim(im: Image.Image) -> Image.Image:
    return im.crop(im.getchannel("A").getbbox())


def square(im: Image.Image, side: int, margin: float) -> Image.Image:
    im = trim(im)
    inner = round(side * (1 - 2 * margin))
    scale = inner / max(im.size)
    scaled = resize(im, (max(1, round(im.size[0] * scale)), max(1, round(im.size[1] * scale))))
    canvas = Image.new("RGBA", (side, side), (0, 0, 0, 0))
    canvas.alpha_composite(scaled, ((side - scaled.size[0]) // 2, (side - scaled.size[1]) // 2))
    return canvas


def recolour(im: Image.Image, rgb: tuple[int, int, int]) -> Image.Image:
    out = Image.new("RGBA", im.size, (*rgb, 0))
    out.putalpha(im.getchannel("A"))
    return out


def main(src: str) -> None:
    im = Image.open(src).convert("RGB")
    rgba = matte(im, background(im))
    cut = split_rows(rgba)
    w, h = rgba.size
    mark = rgba.crop((0, 0, w, cut))
    word = rgba.crop((0, cut, w, h))

    full_dark = trim(rgba)
    light = Image.new("RGBA", rgba.size, (0, 0, 0, 0))
    light.alpha_composite(mark)
    light.alpha_composite(recolour(word, NAVY), (0, cut))
    full_light = light.crop(rgba.getchannel("A").getbbox())

    width = 480
    size = (width, round(full_dark.size[1] * width / full_dark.size[0]))
    outputs = {
        "logo-dark.png": resize(full_dark, size),
        "logo.png": resize(full_light, size),
        "logo-mark.png": square(mark, 256, 0.04),
        "favicon.png": square(mark, 64, 0.03),
    }
    for name, img in outputs.items():
        img.save(os.path.join(OUT, name), optimize=True)
        print(f"docs/assets/{name} {img.size[0]}x{img.size[1]}")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    main(sys.argv[1])
