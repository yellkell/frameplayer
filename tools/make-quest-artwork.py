#!/usr/bin/env python3
"""Generates FramePlayer's Meta Horizon Store artwork into assets/quest/.

Sizes and rules from Meta's asset guidelines
(developers.meta.com/horizon/resources/asset-guidelines): hero 3000x900,
covers 2560x1440 / 1440x1440 / 1008x1440, mini landscape 1080x360 and a
512x512 icon as 24-bit PNG; a transparent logo and a 180x180 spatialized
icon as 32-bit PNG. The title reads exactly "FramePlayer" and is the only
text; it stays out of the corners and the top and bottom fifths; the icon
has square corners and no transparency. Same look as tools/make-artwork.py
(the Steam Frame art), set in Inter like the app.

Needs Pillow:  python3 tools/make-quest-artwork.py
"""
from pathlib import Path
from PIL import Image, ImageChops, ImageDraw, ImageFilter, ImageFont

ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / "assets" / "quest"
FONT = ROOT / "crates" / "frameplayer" / "assets" / "fonts" / "Inter-Bold.ttf"
TOP, BOTTOM = (10, 14, 28), (34, 22, 70)
ACCENT, ACCENT2 = (92, 160, 255), (170, 110, 255)
SS = 2  # drawn at twice the size, then scaled down: smooth rings


def gradient(w, h):
    top = Image.new("RGB", (1, 2))
    top.putpixel((0, 0), TOP)
    top.putpixel((0, 1), BOTTOM)
    return top.resize((w, h), Image.BILINEAR)


def glow(img, cx, cy, r, color, alpha=90):
    layer = Image.new("RGBA", img.size, (0, 0, 0, 0))
    ImageDraw.Draw(layer).ellipse((cx - r, cy - r, cx + r, cy + r), fill=color + (alpha,))
    img.alpha_composite(layer.filter(ImageFilter.GaussianBlur(r * 0.6)))


def rings(img, cx, cy, size, alpha, width_frac=0.045, blur=0.0):
    """The two lenses of the mark, alone (for faint echoes behind)."""
    layer = Image.new("RGBA", img.size, (0, 0, 0, 0))
    d = ImageDraw.Draw(layer)
    r, off, w = size * 0.32, size * 0.17, max(2, int(size * width_frac))
    d.ellipse((cx - off - r, cy - r, cx - off + r, cy + r), outline=ACCENT + (alpha,), width=w)
    d.ellipse((cx + off - r, cy - r, cx + off + r, cy + r), outline=ACCENT2 + (alpha,), width=w)
    if blur:
        layer = layer.filter(ImageFilter.GaussianBlur(blur))
    img.alpha_composite(layer)


def mark(img, cx, cy, size):
    """Two overlapping lenses (stereo) with a play triangle, softly lit."""
    glow(img, cx - size * 0.17, cy, size * 0.36, ACCENT, 60)
    glow(img, cx + size * 0.17, cy, size * 0.36, ACCENT2, 60)
    rings(img, cx, cy, size, 255)
    t = size * 0.16
    ImageDraw.Draw(img).polygon(
        [(cx - t * 0.6, cy - t), (cx - t * 0.6, cy + t), (cx + t, cy)], fill=(255, 255, 255, 255)
    )


def title_width(height):
    font = ImageFont.truetype(str(FONT), int(height))
    return ImageDraw.Draw(Image.new("RGB", (1, 1))).textlength("FramePlayer", font=font)


def title(img, x, y, height, anchor="lm"):
    """FramePlayer: "Frame" white, "Player" in the accent blue."""
    d = ImageDraw.Draw(img)
    font = ImageFont.truetype(str(FONT), int(height))
    d.text((x, y), "Frame", font=font, fill=(255, 255, 255, 255), anchor=anchor)
    w = d.textlength("Frame", font=font)
    d.text((x + w, y), "Player", font=font, fill=ACCENT + (255,), anchor=anchor)


def background(w, h):
    img = gradient(w, h).convert("RGBA")
    glow(img, int(w * 0.78), int(h * 0.28), int(min(w, h) * 0.5), ACCENT2)
    glow(img, int(w * 0.18), int(h * 0.82), int(min(w, h) * 0.45), ACCENT)
    # Faint, large echoes of the lenses, for depth.
    rings(img, w * 0.5, h * 0.5, max(w, h) * 1.1, 26, width_frac=0.012, blur=min(w, h) * 0.004)
    return img


def add_grain(img, strength=12):
    """A little noise (about +/-3 levels) so the big gradients don't band."""
    noise = Image.effect_noise(img.size, 40).convert("L")
    up = noise.point(lambda v: max(0, v - 128) // strength)
    down = noise.point(lambda v: max(0, 128 - v) // strength)
    return Image.merge("RGB", [ImageChops.subtract(ImageChops.add(c, up), down) for c in img.split()])


def render(w, h, draw, transparent=False):
    """Draws at SS x the size, scales down to (w, h)."""
    big = (Image.new("RGBA", (w * SS, h * SS), (0, 0, 0, 0)) if transparent else background(w * SS, h * SS))
    draw(big, w * SS, h * SS)
    small = big.resize((w, h), Image.LANCZOS)
    return small if transparent else add_grain(small.convert("RGB"))


def stacked(img, w, h, mark_size, title_h):
    """Mark above the title, the pair centred in the middle three fifths."""
    gap = mark_size * 0.12
    block = mark_size * 0.72 + gap + title_h
    top = (h - block) / 2
    mark(img, w / 2, top + mark_size * 0.36, mark_size)
    title(img, w / 2 - title_width(title_h) / 2, top + mark_size * 0.72 + gap + title_h / 2, title_h)


def side_by_side(img, w, h, mark_size, title_h):
    """Mark left of the title, the pair centred."""
    gap = mark_size * 0.18
    total = mark_size * 0.98 + gap + title_width(title_h)
    x0 = (w - total) / 2
    mark(img, x0 + mark_size * 0.49, h / 2, mark_size)
    title(img, x0 + mark_size * 0.98 + gap, h / 2 + title_h * 0.04, title_h)


def main():
    OUT.mkdir(parents=True, exist_ok=True)
    s = SS
    assets = {
        "hero-cover-3000x900.png": (3000, 900, lambda i, w, h: side_by_side(i, w, h, 420 * s, 190 * s)),
        "cover-landscape-2560x1440.png": (2560, 1440, lambda i, w, h: stacked(i, w, h, 520 * s, 190 * s)),
        "cover-square-1440x1440.png": (1440, 1440, lambda i, w, h: stacked(i, w, h, 470 * s, 150 * s)),
        "cover-portrait-1008x1440.png": (1008, 1440, lambda i, w, h: stacked(i, w, h, 400 * s, 118 * s)),
        "mini-landscape-1080x360.png": (1080, 360, lambda i, w, h: side_by_side(i, w, h, 170 * s, 76 * s)),
        "trailer-cover-2560x1440.png": (2560, 1440, lambda i, w, h: stacked(i, w, h, 520 * s, 190 * s)),
        "icon-512.png": (512, 512, lambda i, w, h: mark(i, w / 2, h / 2, 440 * s)),
    }
    for name, (w, h, draw) in assets.items():
        render(w, h, draw).save(OUT / name)

    logo = render(2400, 600, lambda i, w, h: side_by_side(i, w, h, 470 * s, 210 * s), transparent=True)
    logo.save(OUT / "logo-transparent.png")
    spatial = render(180, 180, lambda i, w, h: mark(i, w / 2, h / 2, 132 * s), transparent=True)
    spatial.save(OUT / "spatialized-icon-180.png")

    for f in sorted(OUT.iterdir()):
        im = Image.open(f)
        print(f.name, im.size, im.mode)


if __name__ == "__main__":
    main()
