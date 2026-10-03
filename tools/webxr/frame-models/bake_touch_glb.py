#!/usr/bin/env python3
"""Turn the Steam Frame controller models into Quest Touch look-alikes.

Input: a directory written by IWFDK's tools/frame-models on the headset:
left.glb and right.glb (SteamVR's render models, verbatim) and
frame-controller-models.json (how each node moves with each input, and
where the model sits in the grip space).

Output: left.glb and right.glb that WebXR pages load in place of the
`oculus-touch-v3` / `oculus-touch` models from @webxr-input-profiles: the
model sits in the grip space, and every moving part is wrapped in the nodes
those profiles animate (`<response>_value`, with `_min` / `_max` siblings
holding the rest and fully-pressed poses). Pages that animate Touch models
(three.js XRControllerModelFactory, IWSDK) then move the Frame's trigger,
grip, stick and A/B as the gamepad reports them.

Nothing extracted is distributed: run this on your own extraction.

Usage: bake_touch_glb.py IN_DIR OUT_DIR
"""

import json
import math
import struct
import sys
from pathlib import Path

# valve-frame component -> Touch visual response (button-like inputs).
BUTTONS = {
    "xr-standard-trigger": "xr_standard_trigger_pressed",
    "xr-standard-squeeze": "xr_standard_squeeze_pressed",
    "xr-standard-thumbstick": "xr_standard_thumbstick_pressed",
    "a-button": "a_button_pressed",
    "b-button": "b_button_pressed",
    "x-button": "x_button_pressed",
    "y-button": "y_button_pressed",
    "menu": "menu_pressed",
}
STICK_X = "xr_standard_thumbstick_xaxis_pressed"
STICK_Y = "xr_standard_thumbstick_yaxis_pressed"


# ---- quaternions ([x, y, z, w]) ----------------------------------------------

def qmul(a, b):
    ax, ay, az, aw = a
    bx, by, bz, bw = b
    return [
        aw * bx + ax * bw + ay * bz - az * by,
        aw * by - ax * bz + ay * bw + az * bx,
        aw * bz + ax * by - ay * bx + az * bw,
        aw * bw - ax * bx - ay * by - az * bz,
    ]


def qinv(q):
    x, y, z, w = q
    n = x * x + y * y + z * z + w * w or 1.0
    return [-x / n, -y / n, -z / n, w / n]


def qnorm(q):
    n = math.sqrt(sum(c * c for c in q)) or 1.0
    return [c / n for c in q]


def qrotate(q, v):
    p = qmul(qmul(q, [v[0], v[1], v[2], 0.0]), qinv(q))
    return p[:3]


def from_matrix(m):
    """Column-major glTF matrix -> (translation, rotation, scale)."""
    t = [m[12], m[13], m[14]]
    cols = [m[0:3], m[4:7], m[8:11]]
    s = [math.sqrt(sum(c * c for c in col)) or 1.0 for col in cols]
    r = [[cols[j][i] / s[j] for j in range(3)] for i in range(3)]  # row i, col j
    tr = r[0][0] + r[1][1] + r[2][2]
    if tr > 0:
        k = math.sqrt(tr + 1.0) * 2
        q = [(r[2][1] - r[1][2]) / k, (r[0][2] - r[2][0]) / k, (r[1][0] - r[0][1]) / k, k / 4]
    elif r[0][0] > r[1][1] and r[0][0] > r[2][2]:
        k = math.sqrt(1.0 + r[0][0] - r[1][1] - r[2][2]) * 2
        q = [k / 4, (r[0][1] + r[1][0]) / k, (r[0][2] + r[2][0]) / k, (r[2][1] - r[1][2]) / k]
    elif r[1][1] > r[2][2]:
        k = math.sqrt(1.0 + r[1][1] - r[0][0] - r[2][2]) * 2
        q = [(r[0][1] + r[1][0]) / k, k / 4, (r[1][2] + r[2][1]) / k, (r[0][2] - r[2][0]) / k]
    else:
        k = math.sqrt(1.0 + r[2][2] - r[0][0] - r[1][1]) * 2
        q = [(r[0][2] + r[2][0]) / k, (r[1][2] + r[2][1]) / k, k / 4, (r[1][0] - r[0][1]) / k]
    return t, qnorm(q), s


# ---- GLB ------------------------------------------------------------------------

def read_glb(path):
    data = Path(path).read_bytes()
    magic, version, _ = struct.unpack_from("<4sII", data, 0)
    if magic != b"glTF" or version != 2:
        raise SystemExit(f"{path}: not a glTF 2 binary")
    off, gltf, binary = 12, None, b""
    while off < len(data):
        length, kind = struct.unpack_from("<I4s", data, off)
        chunk = data[off + 8: off + 8 + length]
        if kind == b"JSON":
            gltf = json.loads(chunk)
        elif kind == b"BIN\0":
            binary = chunk
        off += 8 + length
    return gltf, binary


def write_glb(path, gltf, binary):
    js = json.dumps(gltf, separators=(",", ":")).encode()
    js += b" " * (-len(js) % 4)
    binary += b"\0" * (-len(binary) % 4)
    chunks = struct.pack("<I4s", len(js), b"JSON") + js
    if binary:
        chunks += struct.pack("<I4s", len(binary), b"BIN\0") + binary
    Path(path).write_bytes(struct.pack("<4sII", b"glTF", 2, 12 + len(chunks)) + chunks)


COMPONENT_BYTES = {5120: 1, 5121: 1, 5122: 2, 5123: 2, 5125: 4, 5126: 4}
TYPE_SIZE = {"SCALAR": 1, "VEC2": 2, "VEC3": 3, "VEC4": 4, "MAT4": 16}


def deinterleave(gltf, binary):
    """Gives every accessor in an interleaved buffer view (byteStride) its
    own tightly packed view. SteamVR's models interleave position, normal
    and UV; some loaders' mesh batching (IWSDK's) copies attribute arrays
    whole and runs out of bounds on interleaved ones."""
    out = bytearray(binary)
    views = gltf["bufferViews"]
    for acc in gltf.get("accessors", []):
        if "bufferView" not in acc:
            continue
        view = views[acc["bufferView"]]
        stride = view.get("byteStride")
        elem = COMPONENT_BYTES[acc["componentType"]] * TYPE_SIZE[acc["type"]]
        if not stride or stride == elem:
            continue
        start = view.get("byteOffset", 0) + acc.get("byteOffset", 0)
        packed = b"".join(
            binary[start + i * stride: start + i * stride + elem] for i in range(acc["count"])
        )
        out += bytes(-len(out) % 4)
        views.append({
            "buffer": view.get("buffer", 0),
            "byteOffset": len(out),
            "byteLength": len(packed),
            "target": 34962,
        })
        out += packed
        acc["bufferView"] = len(views) - 1
        acc["byteOffset"] = 0
    gltf["buffers"][0]["byteLength"] = len(out)
    return bytes(out)


# ---- baking -------------------------------------------------------------------

class Model:
    def __init__(self, gltf):
        self.g = gltf
        self.nodes = gltf["nodes"]
        for n in self.nodes:
            if "matrix" in n:
                t, r, s = from_matrix(n.pop("matrix"))
                n["translation"], n["rotation"], n["scale"] = t, r, s
        self.parent = {}
        for i, n in enumerate(self.nodes):
            for c in n.get("children", []):
                self.parent[c] = i

    def find(self, name):
        for i, n in enumerate(self.nodes):
            if n.get("name") == name:
                return i
        return None

    def add(self, name, t=(0, 0, 0), r=(0, 0, 0, 1), children=()):
        self.nodes.append({"name": name, "translation": list(t), "rotation": list(r),
                           "children": list(children)})
        i = len(self.nodes) - 1
        for c in children:
            self.parent[c] = i
        return i

    def attach(self, parent, child):
        self.nodes[parent].setdefault("children", []).append(child)
        self.parent[child] = parent

    def detach(self, child):
        p = self.parent.pop(child, None)
        if p is not None:
            self.nodes[p]["children"].remove(child)
            if not self.nodes[p]["children"]:
                del self.nodes[p]["children"]
        return p

    def wrap(self, node, name):
        """Puts a new node `name` between `node` and its parent, taking over
        the node's place; returns (wrapper, parent)."""
        p = self.parent[node]
        self.detach(node)
        w = self.add(name)
        self.attach(p, w)
        self.attach(w, node)
        return w, p


def pose(p):
    return list(p["position"]), qnorm(list(p["orientation"]))


def bake_hand(gltf, hand):
    m = Model(gltf)
    scene = gltf["scenes"][gltf.get("scene", 0)]
    # One root in the grip space, at the model's measured offset.
    t, r = pose(hand["gripFromModel"]) if hand.get("gripFromModel") else ([0, 0, 0], [0, 0, 0, 1])
    root = m.add("frame-controller", t, r, scene["nodes"])
    scene["nodes"] = [root]

    # Parts hidden at rest (touch indicators). SteamVR has been seen to
    # report every part hidden, which would hide the whole controller:
    # only trust the data when something is visible.
    vis = hand.get("visibleAtRest", {})
    if any(vis.values()):
        for name, visible in vis.items():
            i = m.find(name)
            if i is not None and not visible:
                m.nodes[i]["scale"] = [0, 0, 0]

    done = []
    stick_nodes = set()
    # Sticks first: a stick's click then nests inside its tilt.
    anims = sorted(hand.get("animations", []), key=lambda a: a["kind"] != "stick")
    for a in anims:
        node = m.find(a["node"])
        if node is None or node == root:
            continue
        if a["kind"] == "stick" and a["component"] == "xr-standard-thumbstick":
            rest_t, rest_r = pose(a["rest"])
            inv = qinv(rest_r)

            def delta(p):
                if not p:
                    return None
                ft, fr = pose(p)
                d = [ft[i] - rest_t[i] for i in range(3)]
                return qrotate(inv, d), qmul(inv, fr)

            dl, dr, du, dd = (delta(a.get(k)) for k in ("left", "right", "up", "down"))
            mirror = lambda d: d and ([-c for c in d[0]], qinv(d[1]))
            dl, dr = dl or mirror(dr), dr or mirror(dl)
            du, dd = du or mirror(dd), dd or mirror(du)
            if not (dl and du):
                continue
            # parent -> X (rest + sideways tilt) -> Y (forward/back) -> stick
            x, p = m.wrap(node, STICK_X + "_value")
            y, _ = m.wrap(node, STICK_Y + "_value")
            m.nodes[node]["translation"], m.nodes[node]["rotation"] = [0, 0, 0], [0, 0, 0, 1]

            def full(d):
                return ([rest_t[i] + qrotate(rest_r, d[0])[i] for i in range(3)],
                        qnorm(qmul(rest_r, d[1])))

            lt, lr = full(dl)
            rt, rr = full(dr)
            m.nodes[x]["translation"], m.nodes[x]["rotation"] = list(rest_t), list(rest_r)
            m.attach(p, m.add(STICK_X + "_min", lt, lr))
            m.attach(p, m.add(STICK_X + "_max", rt, rr))
            # WebXR's y axis is -1 pushed forward ("up"): min = up.
            m.attach(x, m.add(STICK_Y + "_min", *du))
            m.attach(x, m.add(STICK_Y + "_max", *dd))
            stick_nodes.add(node)
            done.append("thumbstick")
        elif a["kind"] == "button" and a["component"] in BUTTONS:
            base = BUTTONS[a["component"]]
            rest_t, rest_r = pose(a["rest"])
            on_t, on_r = pose(a["pressed"])
            # A wrapper carries the motion, so a stick (already wrapped)
            # can be pressed too.
            v, p = m.wrap(node, base + "_value")
            if node in stick_nodes:
                # The stick's rest pose is already on its tilt wrappers: the
                # press is a change from rest.
                inv = qinv(rest_r)
                d_t = qrotate(inv, [on_t[i] - rest_t[i] for i in range(3)])
                rest_t, rest_r, on_t, on_r = [0, 0, 0], [0, 0, 0, 1], d_t, qmul(inv, on_r)
            m.nodes[node]["translation"], m.nodes[node]["rotation"] = [0, 0, 0], [0, 0, 0, 1]
            m.nodes[v]["translation"], m.nodes[v]["rotation"] = list(rest_t), list(rest_r)
            m.attach(p, m.add(base + "_min", rest_t, rest_r))
            m.attach(p, m.add(base + "_max", on_t, on_r))
            done.append(a["component"])
    return done


def main():
    if len(sys.argv) != 3:
        raise SystemExit(__doc__)
    src, dst = Path(sys.argv[1]), Path(sys.argv[2])
    models = json.loads((src / "frame-controller-models.json").read_text())
    dst.mkdir(parents=True, exist_ok=True)
    for side in ("left", "right"):
        hand = models["hands"].get(side)
        if not hand:
            print(f"{side}: no model")
            continue
        gltf, binary = read_glb(src / hand["asset"])
        binary = deinterleave(gltf, binary)
        done = bake_hand(gltf, hand)
        write_glb(dst / f"{side}.glb", gltf, binary)
        print(f"{side}: {dst / (side + '.glb')} animates {', '.join(done) or 'nothing'}")


if __name__ == "__main__":
    main()
