#!/usr/bin/env python3
"""Generates FramePlayer's Steam library artwork into assets/steam/.

Sizes follow Steam's non-Steam-game artwork: portrait capsule 600x900,
hero 3840x1240, logo 1280x720 (transparent), wide capsule 920x430 and a
256x256 icon. Needs Pillow and the DejaVu fonts. Re-run after design changes:
    python3 tools/make-artwork.py
"""
from pathlib import Path
from PIL import Image, ImageDraw, ImageFilter, ImageFont

OUT = Path(__file__).resolve().parent.parent / "assets" / "steam"
FONT = "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf"
TOP, BOTTOM = (10, 14, 28), (34, 22, 70)
ACCENT, ACCENT2 = (92, 160, 255), (170, 110, 255)


def gradient(w, h):
    img = Image.new("RGB", (w, h))
    px = img.load()
    for y in range(h):
        t = y / max(h - 1, 1)
        row = tuple(int(TOP[i] + (BOTTOM[i] - TOP[i]) * t) for i in range(3))
        for x in range(w):
            px[x, y] = row
    return img


def glow(img, cx, cy, r, color):
    layer = Image.new("RGBA", img.size, (0, 0, 0, 0))
    ImageDraw.Draw(layer).ellipse((cx - r, cy - r, cx + r, cy + r), fill=color + (90,))
    layer = layer.filter(ImageFilter.GaussianBlur(r * 0.6))
    img.alpha_composite(layer)


def mark(img, cx, cy, size):
    """Two overlapping lenses (stereo) with a play triangle."""
    d = ImageDraw.Draw(img)
    r = size * 0.32
    off = size * 0.17
    w = max(2, int(size * 0.045))
    d.ellipse((cx - off - r, cy - r, cx - off + r, cy + r), outline=ACCENT + (255,), width=w)
    d.ellipse((cx + off - r, cy - r, cx + off + r, cy + r), outline=ACCENT2 + (255,), width=w)
    t = size * 0.16
    d.polygon([(cx - t * 0.6, cy - t), (cx - t * 0.6, cy + t), (cx + t, cy)], fill=(255, 255, 255, 255))


def wordmark(img, x, y, height, anchor="lm"):
    d = ImageDraw.Draw(img)
    font = ImageFont.truetype(FONT, int(height))
    d.text((x, y), "Frame", font=font, fill=(255, 255, 255, 255), anchor=anchor)
    if anchor[0] == "l":
        w = d.textlength("Frame", font=font)
        d.text((x + w, y), "Player", font=font, fill=ACCENT + (255,), anchor=anchor)
    return font


def canvas(w, h):
    img = gradient(w, h).convert("RGBA")
    glow(img, int(w * 0.75), int(h * 0.3), int(min(w, h) * 0.45), ACCENT2)
    glow(img, int(w * 0.2), int(h * 0.8), int(min(w, h) * 0.4), ACCENT)
    return img


def centered_wordmark(img, cx, y, height):
    d = ImageDraw.Draw(img)
    font = ImageFont.truetype(FONT, int(height))
    total = d.textlength("FramePlayer", font=font)
    wordmark(img, cx - total / 2, y, height)


def main():
    OUT.mkdir(parents=True, exist_ok=True)
    p = canvas(600, 900)
    mark(p, 300, 380, 420)
    centered_wordmark(p, 300, 700, 72)
    ImageDraw.Draw(p).text((300, 790), "VR video player", font=ImageFont.truetype(FONT, 30), fill=(190, 196, 215, 255), anchor="mm")
    p.convert("RGB").save(OUT / "portrait.png")

    c = canvas(920, 430)
    mark(c, 170, 215, 250)
    wordmark(c, 320, 215, 66)
    c.convert("RGB").save(OUT / "capsule.png")

    h = canvas(3840, 1240)
    mark(h, 2900, 620, 900)
    h.convert("RGB").save(OUT / "hero.png")

    logo = Image.new("RGBA", (1280, 720), (0, 0, 0, 0))
    mark(logo, 240, 360, 360)
    wordmark(logo, 450, 360, 100)
    logo.save(OUT / "logo.png")

    icon = Image.new("RGBA", (256, 256), (0, 0, 0, 0))
    bg = canvas(256, 256)
    m = Image.new("L", (256, 256), 0)
    ImageDraw.Draw(m).rounded_rectangle((0, 0, 255, 255), radius=52, fill=255)
    icon.paste(bg, (0, 0), m)
    mark(icon, 128, 128, 230)
    icon.save(OUT / "icon.png")
    for f in sorted(OUT.iterdir()):
        print(f, Image.open(f).size)


if __name__ == "__main__":
    main()
