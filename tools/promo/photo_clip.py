"""Turns a still picture into a short side-by-side 3D clip for FramePlayer's
promo: depth from the picture (Depth Anything V2), then a slow push-in and
sway with each eye drawn from its own viewpoint, so near things move against
far ones. Also writes the promo film's frames (left eye, 1920x1080).

    python photo_clip.py PHOTO "Title" OUT_DIR [SECONDS] [FILM_DIR]

Writes OUT_DIR/"Title_LR.mp4" (3840x1080, 30 fps), which FramePlayer reads as
flat side-by-side 3D. Needs torch, transformers, opencv and ffmpeg.
"""
import subprocess
import sys
from pathlib import Path

import cv2
import numpy as np
import torch
from PIL import Image
from transformers import pipeline

EYE_W, EYE_H, FPS = 1920, 1080, 30
MARGIN = 1.12   # crop in this much so moving edges never show
IPD = 0.010     # eye offset, in half-widths at full nearness
CONVERGE = 0.3  # nearness that sits on the screen; nearer pops out
PUSH = 0.07     # how far the camera moves in over the clip
SWAY = 0.025    # side to side
FILM_FRAMES = 90


def depth_of(img):
    """Nearness 0 (far, sky) to 1 (nearest), the picture's size."""
    pipe = pipeline("depth-estimation", model="depth-anything/Depth-Anything-V2-Base-hf",
                    device=0 if torch.cuda.is_available() else -1)
    d = np.asarray(pipe(img)["predicted_depth"].squeeze().float().cpu().numpy())
    d = cv2.resize(d, img.size, interpolation=cv2.INTER_CUBIC)
    lo, hi = np.percentile(d, 1), np.percentile(d, 99.5)
    d = np.clip((d - lo) / (hi - lo), 0, 1)
    # Grow near things a little so their edges stretch the background
    # rather than tearing holes in it.
    d = cv2.dilate(d, np.ones((9, 9), np.uint8))
    return cv2.GaussianBlur(d, (0, 0), 3).astype(np.float32)


def view(src, near, tx, ty, tz):
    """The picture seen from a camera moved by (tx, ty) sideways and tz in."""
    h, w = near.shape
    half = w / 2
    ys, xs = np.mgrid[0:EYE_H, 0:EYE_W].astype(np.float32)
    # Output pixel -> centred, half-width units, inside the crop margin.
    scale = (w / EYE_W) / MARGIN
    px = (xs - EYE_W / 2) * scale / half
    py = (ys - EYE_H / 2) * scale / half
    sx, sy = px.copy(), py.copy()
    for _ in range(4):  # fixed point: find the source pixel that lands here
        d = cv2.remap(near, *pixels(sx, sy, w, h), cv2.INTER_LINEAR,
                      borderMode=cv2.BORDER_REPLICATE)
        sx = px * (1 - tz * d) + tx * (d - CONVERGE)
        sy = py * (1 - tz * d) + ty * d
    return cv2.remap(src, *pixels(sx, sy, w, h), cv2.INTER_CUBIC,
                     borderMode=cv2.BORDER_REFLECT)


def pixels(sx, sy, w, h):
    """Centred half-width units back to pixel maps, as remap wants them."""
    half = w / 2
    return (sx * half + w / 2).astype(np.float32), (sy * half + h / 2).astype(np.float32)


def camera(s):
    """Camera at s in [0, 1]: eased push-in, slow sway."""
    e = 0.5 - 0.5 * np.cos(np.pi * s)
    return SWAY * np.sin(2 * np.pi * (s - 0.25)), -0.006 * e, PUSH * e


def main():
    photo, title, out = Path(sys.argv[1]), sys.argv[2], Path(sys.argv[3])
    seconds = float(sys.argv[4]) if len(sys.argv) > 4 else 20
    film = Path(sys.argv[5]) if len(sys.argv) > 5 else None
    out.mkdir(parents=True, exist_ok=True)
    img = Image.open(photo).convert("RGB")
    if img.width / img.height > 16 / 9:  # crop to 16:9
        w = img.height * 16 // 9
        img = img.crop(((img.width - w) // 2, 0, (img.width + w) // 2, img.height))
    else:
        h = img.width * 9 // 16
        img = img.crop((0, (img.height - h) // 2, img.width, (img.height + h) // 2))
    near = depth_of(img)
    src = cv2.cvtColor(np.asarray(img), cv2.COLOR_RGB2BGR)
    Image.fromarray((near * 255).astype(np.uint8)).save(out / f"{title}-depth.png")

    n = int(seconds * FPS)
    enc = subprocess.Popen(
        ["ffmpeg", "-y", "-loglevel", "error", "-f", "rawvideo", "-pix_fmt", "bgr24",
         "-s", f"{EYE_W * 2}x{EYE_H}", "-r", str(FPS), "-i", "-",
         "-c:v", "libx264", "-preset", "slow", "-crf", "17", "-pix_fmt", "yuv420p",
         "-movflags", "+faststart", str(out / f"{title}_LR.mp4")],
        stdin=subprocess.PIPE)
    for i in range(n):
        tx, ty, tz = camera(i / (n - 1))
        left = view(src, near, tx - IPD, ty, tz)
        right = view(src, near, tx + IPD, ty, tz)
        enc.stdin.write(np.hstack([left, right]).tobytes())
        if i % 60 == 0:
            print(f"{title}: {i}/{n}", flush=True)
    enc.stdin.close()
    enc.wait()

    if film:  # the film shows the same move, quicker
        film.mkdir(parents=True, exist_ok=True)
        for i in range(FILM_FRAMES):
            tx, ty, tz = camera(i / (FILM_FRAMES - 1))
            cv2.imwrite(str(film / f"{i:04d}.jpg"), view(src, near, tx, ty, tz),
                        [cv2.IMWRITE_JPEG_QUALITY, 90])
    print(title, "done")


if __name__ == "__main__":
    main()
