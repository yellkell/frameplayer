#!/usr/bin/env python3
"""Adds VR180 Spherical Video V2 metadata to an MP4, like VR180 cameras and
YouTube do: st3d (left-right stereo) and sv3d with an equirectangular
projection cropped to 180 degrees (left/right bounds of 0.25 each).

    python3 tools/make-vr180-fixture.py in.mp4 out.mp4

The input must keep moov after mdat (ffmpeg's default without faststart),
so growing moov does not move any sample data.
"""
import struct
import sys

CONTAINERS = {b"moov", b"trak", b"mdia", b"minf", b"stbl"}


def box(kind, payload):
    return struct.pack(">I", 8 + len(payload)) + kind + payload


def full_box(kind, payload, version=0, flags=0):
    return box(kind, struct.pack(">I", (version << 24) | flags) + payload)


def vr180_boxes():
    st3d = full_box(b"st3d", bytes([2]))  # 2 = left-right
    svhd = full_box(b"svhd", b"frameplayer fixture\0")
    prhd = full_box(b"prhd", struct.pack(">iii", 0, 0, 0))
    quarter = 1 << 30  # 0.25 in 0.32 fixed point
    equi = full_box(b"equi", struct.pack(">IIII", 0, 0, quarter, quarter))
    sv3d = box(b"sv3d", svhd + box(b"proj", prhd + equi))
    return st3d + sv3d


def rewrite(data, inject):
    """Returns `data` (a sequence of boxes) with the boxes injected into the
    first visual sample entry, fixing every ancestor's size."""
    out = b""
    i = 0
    while i < len(data):
        size, kind = struct.unpack(">I4s", data[i:i + 8])
        body = data[i + 8:i + size]
        if kind in CONTAINERS:
            body = rewrite(body, inject)
        elif kind == b"stsd":
            # full box header + entry count, then sample entries.
            head, entries = body[:8], body[8:]
            esize, ekind = struct.unpack(">I4s", entries[:8])
            if ekind in (b"avc1", b"hvc1", b"hev1", b"av01", b"vp09"):
                entry = entries[:esize] + inject
                entries = struct.pack(">I", len(entry)) + entry[4:] + entries[esize:]
            body = head + entries
        out += struct.pack(">I", 8 + len(body)) + kind + body
        i += size
    return out


def main():
    src, dst = sys.argv[1], sys.argv[2]
    data = open(src, "rb").read()
    order = []
    i = 0
    while i < len(data):
        size, kind = struct.unpack(">I4s", data[i:i + 8])
        order.append(kind)
        i += size
    if order.index(b"moov") < order.index(b"mdat"):
        sys.exit("moov must come after mdat (do not use -movflags faststart)")
    open(dst, "wb").write(rewrite(data, vr180_boxes()))


if __name__ == "__main__":
    main()
