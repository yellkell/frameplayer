#!/usr/bin/env python3
"""Generates Chromium XR's Steam library artwork into tools/webxr/frame-title/steam/.

The same sizes and layout as FramePlayer's (tools/make-artwork.py): portrait
capsule 600x900, hero 3840x1240, logo 1280x720 (transparent), wide capsule
920x430 and a 256x256 icon. The mark is the one on yellkell.com/frame: a
headset whose two lenses are globes. Needs Pillow; uses FramePlayer's Inter.
    python3 tools/webxr/make-chromium-xr-artwork.py
"""
import math
from pathlib import Path
from PIL import Image, ImageChops, ImageDraw, ImageFilter, ImageFont

ROOT = Path(__file__).resolve().parent.parent.parent
OUT = ROOT / "tools" / "webxr" / "frame-title" / "steam"
FONT = ROOT / "crates" / "frameplayer" / "assets" / "fonts" / "Inter-Bold.ttf"
TOP, BOTTOM = (7, 16, 24), (12, 26, 52)
TEAL, BLUE = (46, 230, 197), (63, 169, 255)
SS = 4  # supersampling for the mark


def gradient(w, h):
    col = Image.linear_gradient("L").resize((1, 256))
    img = Image.new("RGB", (w, h))
    top, bottom = Image.new("RGB", (w, h), TOP), Image.new("RGB", (w, h), BOTTOM)
    return Image.composite(bottom, top, col.resize((w, h)))


def glow(img, cx, cy, r, color, alpha=90):
    layer = Image.new("RGBA", img.size, (0, 0, 0, 0))
    ImageDraw.Draw(layer).ellipse((cx - r, cy - r, cx + r, cy + r), fill=color + (alpha,))
    layer = layer.filter(ImageFilter.GaussianBlur(r * 0.6))
    img.alpha_composite(layer)


def visor_outline(steps=24):
    """The headset outline (the website's SVG path, in its 0..100 box)."""
    pts = []

    def arc(cx, cy, r, a0, a1):
        for k in range(steps + 1):
            a = math.radians(a0 + (a1 - a0) * k / steps)
            pts.append((cx + r * math.cos(a), cy + r * math.sin(a)))

    def quad(p0, c, p1):
        for k in range(1, steps + 1):
            t = k / steps
            pts.append(((1 - t) ** 2 * p0[0] + 2 * (1 - t) * t * c[0] + t * t * p1[0],
                        (1 - t) ** 2 * p0[1] + 2 * (1 - t) * t * c[1] + t * t * p1[1]))

    arc(27, 46, 15, 180, 270)          # top-left corner
    arc(73, 46, 15, 270, 360)          # top-right
    arc(73, 55, 15, 0, 90)             # bottom-right
    pts.append((62, 70))
    quad((62, 70), (58.5, 70), (56.6, 66.8))
    pts.append((54.6, 63.4))
    quad((54.6, 63.4), (50, 56.5), (45.4, 63.4))   # the nose bridge
    pts.append((43.4, 66.8))
    quad((43.4, 66.8), (41.5, 70), (38, 70))
    arc(27, 55, 15, 90, 180)           # bottom-left
    pts.append(pts[0])
    return pts


def mark(img, cx, cy, size):
    """Paste the mark, `size` px across, centred on (cx, cy)."""
    n = int(size * SS)
    s = n / 100.0  # the mark's 0..100 box, with the visor spanning 12..88
    color = Image.new("L", (n, n), 0)
    white = Image.new("L", (n, n), 0)
    dc, dw = ImageDraw.Draw(color), ImageDraw.Draw(white)
    pts = []
    for x, y in visor_outline():
        if not pts or (x * s, y * s) != pts[-1]:
            pts.append((x * s, y * s))
    w = 5.5 * s
    dc.line(pts, fill=255, width=int(w))
    # Round joins (Pillow's own leave hairline gaps between segments).
    for x, y in pts:
        dc.ellipse((x - w / 2, y - w / 2, x + w / 2, y + w / 2), fill=255)
    for x in (34, 66):
        r = 10.5 * s
        dc.ellipse((x * s - r, 49 * s - r, x * s + r, 49 * s + r), outline=255, width=int(3.5 * s))
        dw.ellipse((x * s - 4.2 * s, 49 * s - r, x * s + 4.2 * s, 49 * s + r), outline=255, width=int(2 * s))
        dw.line([((x - 10.5) * s, 49 * s), ((x + 10.5) * s, 49 * s)], fill=255, width=int(2 * s))
    # Teal to blue across the mark, white globe lines on top.
    # Top-left 0 to bottom-right 255: the mean of a vertical and a horizontal ramp.
    v = Image.linear_gradient("L").resize((n, n))
    ramp = ImageChops.add(v, v.transpose(Image.Transpose.ROTATE_90), scale=2.0)
    grad = Image.composite(Image.new("RGBA", (n, n), BLUE + (255,)), Image.new("RGBA", (n, n), TEAL + (255,)), ramp)
    layer = Image.new("RGBA", (n, n), (0, 0, 0, 0))
    layer.paste(grad, (0, 0), color)
    layer.paste(Image.new("RGBA", (n, n), (255, 255, 255, 255)), (0, 0), white)
    layer = layer.resize((int(size), int(size)), Image.LANCZOS)
    # A soft teal glow under it.
    halo = Image.new("RGBA", img.size, (0, 0, 0, 0))
    halo.paste(Image.new("RGBA", layer.size, TEAL + (110,)), (int(cx - size / 2), int(cy - size / 2)), layer)
    img.alpha_composite(halo.filter(ImageFilter.GaussianBlur(size * 0.05)))
    img.alpha_composite(layer, (int(cx - size / 2), int(cy - size / 2)))


def wordmark(img, x, y, height, anchor="lm"):
    d = ImageDraw.Draw(img)
    font = ImageFont.truetype(str(FONT), int(height))
    d.text((x, y), "Chromium ", font=font, fill=(255, 255, 255, 255), anchor=anchor)
    w = d.textlength("Chromium ", font=font)
    d.text((x + w, y), "XR", font=font, fill=TEAL + (255,), anchor=anchor)
    return d.textlength("Chromium XR", font=font)


def canvas(w, h):
    img = gradient(w, h).convert("RGBA")
    glow(img, int(w * 0.75), int(h * 0.3), int(min(w, h) * 0.45), BLUE)
    glow(img, int(w * 0.2), int(h * 0.8), int(min(w, h) * 0.4), TEAL, 70)
    return img


def main():
    OUT.mkdir(parents=True, exist_ok=True)
    font = ImageFont.truetype(str(FONT), 64)

    p = canvas(600, 900)
    mark(p, 300, 380, 470)
    d = ImageDraw.Draw(p)
    total = d.textlength("Chromium XR", font=ImageFont.truetype(str(FONT), 64))
    wordmark(p, 300 - total / 2, 690, 64)
    d.text((300, 785), "WebXR browser", font=ImageFont.truetype(str(FONT.with_name("Inter-Medium.ttf")), 30),
           fill=(190, 205, 215, 255), anchor="mm")
    p.convert("RGB").save(OUT / "portrait.png")

    c = canvas(920, 430)
    mark(c, 175, 215, 270)
    wordmark(c, 320, 215, 64)
    c.convert("RGB").save(OUT / "capsule.png")

    h = canvas(3840, 1240)
    mark(h, 2900, 620, 1000)
    h.convert("RGB").save(OUT / "hero.png")

    logo = Image.new("RGBA", (1280, 720), (0, 0, 0, 0))
    mark(logo, 230, 360, 380)
    wordmark(logo, 430, 360, 100)
    logo.save(OUT / "logo.png")

    icon = Image.new("RGBA", (256, 256), (0, 0, 0, 0))
    bg = canvas(256, 256)
    m = Image.new("L", (256, 256), 0)
    ImageDraw.Draw(m).rounded_rectangle((0, 0, 255, 255), radius=58, fill=255)
    icon.paste(bg, (0, 0), m)
    mark(icon, 128, 128, 236)
    icon.save(OUT / "icon.png")
    for f in sorted(OUT.iterdir()):
        print(f, Image.open(f).size)


if __name__ == "__main__":
    main()
