#!/usr/bin/env python3
"""Build the "Steam Frame controller models" extension for Chromium XR.

The extension redirects WebXR pages' requests for the Quest Touch
controller models (@webxr-input-profiles' `oculus-touch-v3` and
`oculus-touch` left/right.glb, from any CDN path) to the Frame models baked
by bake_touch_glb.py, which it carries. Chromium XR's launcher loads it from
~/chromium-xr-frame/frame-models when that folder exists.

Usage: make_extension.py BAKED_DIR OUT_DIR
"""

import json
import shutil
import sys
from pathlib import Path

PROFILES = r"(oculus-touch|oculus-touch-v3)"
URL = r"^https?://[^?#]*/profiles/" + PROFILES + r"/{side}\.glb([?#].*)?$"


def main():
    if len(sys.argv) != 3:
        raise SystemExit(__doc__)
    baked, out = Path(sys.argv[1]), Path(sys.argv[2])
    (out / "models").mkdir(parents=True, exist_ok=True)
    rules = []
    for i, side in enumerate(("left", "right"), start=1):
        glb = baked / f"{side}.glb"
        if not glb.exists():
            raise SystemExit(f"{glb} is missing")
        shutil.copyfile(glb, out / "models" / f"{side}.glb")
        rules.append({
            "id": i,
            "priority": 1,
            "action": {"type": "redirect", "redirect": {"extensionPath": f"/models/{side}.glb"}},
            "condition": {
                "regexFilter": URL.format(side=side),
                "resourceTypes": ["xmlhttprequest", "other"],
            },
        })
    manifest = {
        "manifest_version": 3,
        "name": "Steam Frame controller models",
        "version": "1.0",
        "description": "Shows Steam Frame controllers where WebXR pages ask for Quest Touch models.",
        "permissions": ["declarativeNetRequestWithHostAccess"],
        "host_permissions": ["<all_urls>"],
        "declarative_net_request": {
            "rule_resources": [{"id": "models", "enabled": True, "path": "rules.json"}]
        },
        "web_accessible_resources": [
            {"resources": ["models/*"], "matches": ["<all_urls>"]}
        ],
    }
    (out / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    (out / "rules.json").write_text(json.dumps(rules, indent=2) + "\n")
    print(f"extension in {out}")


if __name__ == "__main__":
    main()
