# frameplayer-probe

An on-device self-test for the Steam Frame. It answers as many of the
`[verify]` questions in [`docs/platform-notes.md`](../../docs/platform-notes.md)
as can be answered automatically and writes **one report** that the owner
can drag into a chat or paste into a GitHub issue. Nobody has to type
anything, and nothing is uploaded anywhere.

```
frameplayer-probe               # full test, launched from the Steam library (headset on)
frameplayer-probe --headless    # over SSH, nobody wearing the headset
```

Output:

* `~/frameplayer-probe-report.txt`: a short summary with pass/fail/unknown per
  check, the answers grouped by platform-notes row (P1…P18, I1…I14), and one line per finding.
* `~/frameplayer-probe-report.json`: the machine-readable report (schema below).
* stdout: the summary followed by the JSON. With `--summary-only`, stdout gets only the summary.
* stderr: one progress line per check.

The report is rewritten after every check, so a run that is killed part way
through still leaves the results gathered so far. Exit code: 0 when the
report was written, 1 when the report could not be written, 2 for bad arguments.

Options: `--with-session` (create an OpenXR session even with
`--headless`), `--only id,id`, `--out DIR`, `--summary-only`, `--list`.

## Checks

Every check runs in a child process of the probe
(`frameplayer-probe --check <id> --mode …`) under a time limit. A segfault
or hang in a GPU, XR or video driver costs only that check. The report then
records `crashed` with the signal name, or `timeout`, along with the stderr
tail and any partial results the child published before it died.

| id | What it does | platform-notes rows |
|---|---|---|
| `video_decode` | `/dev/video*`, `/dev/media*`, `/dev/dri/*`, `/dev/dma_heap/*` (mode, owner, group, rw access); `VIDIOC_QUERYCAP`; `ENUM_FMT` on both queues and the CAPTURE formats offered *per coded format*, which is where UBWC `QC08C`/`QC10C` shows up; `ENUM_FRAMESIZES`; real decodes of six embedded clips (HEVC 8-bit, H.264, VP9, AV1 at 256²; HEVC Main10 at 3840×1920 and 7680×3840) through fp-video's `V4l2Decoder`. Records frames, ms/frame, output fourcc, DRM modifier, plane offsets and pitches, DMA-BUF fds (`EXPBUF`) and a luma sample that proves real pixels came out. Repeats the HEVC decode with UBWC output allowed. | P2, P3, P4, P17 (8K accepted) |
| `vulkan` | ash, no XR: instance, devices, driver id/name/info, conformance version, API version, every device extension, the zero-copy and Vulkan Video extensions, video queue families, `vkGetPhysicalDeviceVideoCapabilitiesKHR` for H.264 High, HEVC Main/Main10 and AV1 8/10-bit, DRM modifier lists for R8/RG8/R16/RG16/NV12/P010-2plane, and whether LINEAR and `QCOM_COMPRESSED` are importable as sampled DMA-BUF images. | P4, P5, P9 |
| `dmabuf_import` | **The most important result.** Decodes a frame through V4L2 and keeps it. Creates a Vulkan 1.3 device the same way fp-xr does. Imports each plane step by step and records the exact `VkResult` of the step that fails. Tries a 2-plane import, which is the route UBWC needs. Then runs the real fp-gfx path (`Renderer::upload_video_frame` + `convert_video`). A linear buffer from `/dev/dma_heap/system` serves as a control. | P4 |
| `openxr` | Loader (`libopenxr_loader.so`, then `.so.1`), runtime name and version, API layers, instance extensions, HMD system properties, per-eye recommended/max size, blend modes, Vulkan requirements, eye-gaze and hand-tracking support. Suggests **every component path individually** for the Frame, Index and simple profiles (fp-xr's list plus about 27 extra Frame candidates per hand) and records which ones the runtime accepts. With a session allowed, it goes through fp-xr's real `XrContext → VulkanContext → XrSession` path to read swapchain formats, refresh rates, reference spaces and lifecycle events. | P1, P6, P8, P10, P11, P12, P13, I11 |
| `interactive` | Default mode only. Runs fp-xr, fp-gfx and fp-ui together. Measures frame pacing for 10 s on a 3840×1920 equirect sphere, then for 5 s at the highest offered refresh rate. Then walks through A/B/X/Y, menu, view, triggers, grips, bumpers, stick clicks and moves, and the D-pad, followed by a pinch with each hand and a look at left/right targets (eye gaze). Each step skips itself after 10 s. Records which inputs fired and the active interaction profile. Ends with "Done! The report is saved. You can take the headset off." | P1, P10, P11 |
| `system` | `/etc/os-release` (selected keys), SteamOS atomupd manifest, kernel, glibc, newest `GLIBC_`/`GLIBCXX_` symbol, which of libstdc++/libvulkan/libopenxr_loader/libpipewire/libasound/libdrm/libgbm/libEGL `dlopen` finds and where, CPU (cpuinfo, devicetree model), memory, uid and groups (video/render/audio/input), Steam Linux Runtime container markers, privacy-filtered environment, OpenXR `active_runtime.json` search path, Vulkan ICDs, SteamVR build id. | P7, P9, P18, I3, I11 |
| `audio` | `/proc/asound/cards`, PipeWire/Pulse sockets, and the ALSA `default` PCM through `dlopen("libasound.so.2")` (opens it, negotiates 48 kHz stereo f32, reads back rate, buffer and period, closes it again; nothing is played). The `cpal` feature adds cpal's device list. | P15 |
| `storage` | Mount table summary through fp-sources' `parse_mountinfo` and `removable_mounts`, `/run/media` layout, microSD and USB block devices, free space. | P14 |
| `network` | Interface kinds (no addresses), an SSDP `M-SEARCH ssdp:all` using fp-sources' request builder and parser (any reply within 3 s?), and whether TCP 23554 and 8642 can be bound. | P16 |
| `devkit` | devkit service on `127.0.0.1:32000`, Steam DevTools on `:8080`, `~/devkit-game`, `~/devkit-utils/steam-client-create-shortcut`, GNU coreutils/tar/zstd, thermal zones, kgsl and devfreq, Steam log dir. | I1, I2, I4, I5, I6, I10, I14 |

Checks the probe cannot answer: P13 (cylinder orientation needs a human
judgement), P17 (2 h thermal soak; use `tools/perf-capture.sh`), I7–I9,
I12 and I13 (desktop or tool side).

## Report schema (version 1)

```jsonc
{
  "schema": "frameplayer-probe-report",
  "schema_version": 1,           // bumped on incompatible changes
  "probe_version": "0.1.0",
  "generated_at": "2026-10-03T12:00:00Z",
  "mode": "headless" | "interactive",
  "arch": "aarch64",
  "duration_ms": 41234,
  "summary": {
    "counts": { "pass": 6, "fail": 1, "unknown": 2, "skipped": 1 },
    "checks": ["video_decode: PASS 6 of 6 test clips decode …", …],
    "answers": [ { "ref": "P4", "status": "pass|fail|partial|unknown", "summary": "…" }, … ]
  },
  // then one object per check, most important first:
  // video_decode, vulkan, dmabuf_import, openxr, interactive, system, audio, storage, network, devkit
  "video_decode": {
    "title": "…",
    "status": "pass|fail|unknown|skipped|crashed|timeout",
    "summary": "one line",
    "duration_ms": 812,
    "isolation": "child" | "in_process",
    "exit": { "code": null, "signal": 11, "signal_name": "SIGSEGV" },   // only when it died
    "stderr_tail": "…",                                                 // only when not passing
    "findings": [ { "id": "decode_hevc_256x256", "status": "pass", "summary": "…", "refs": ["P2"] } ],
    "data": { … check-specific details … }
  }
}
```

`fp_probe::report::Report::from_json` parses a report back for tooling.

### Privacy and size

The report is meant to be posted publicly, so:

* No IP or MAC addresses, hostnames, serial numbers, machine-id or Wi-Fi
  names are collected. Network data is limited to interface kinds and "has
  LAN IPv4: yes/no".
* Environment variables: values only for an allowlist (`XR_RUNTIME_JSON` and
  other paths, with the home directory shown as `~`; `PRESSURE_VESSEL_*`;
  numeric `SteamAppId`-style ids). The other relevant variables are listed by
  name only.
* The login name is shown only when it is a platform default (`deck`,
  `steam`, …). Removable-media labels become `<label>`.
* A final `redact` pass over the JSON and the text acts as a second safety
  net. It replaces IPv4/IPv6 addresses (except loopback and the SSDP group),
  MAC addresses, hex or base64 runs of 32+ characters, `/home/<user>` and
  the user's name.
* Size: the JSON targets less than 40 KB and is capped so that summary plus
  JSON fit one GitHub issue body (64 000 characters). When needed, the details
  of the least important checks are dropped first, and a note says so.

## How the desktop installer should run it (`frameplayer-install probe`)

The release build (`cargo build -p fp-probe --release --target
aarch64-unknown-linux-gnu`) ships next to the player in the tarball:

```
versions/<ver>/bin/frameplayer
versions/<ver>/bin/frameplayer-probe      # add to tools/release.sh: install -m 0755 … "$V/bin/"
```

so on the headset it is reachable as
`~/devkit-game/frameplayer/current/bin/frameplayer-probe`. The launcher
script keeps `current` pointing at the active version. The probe
needs no bundled libraries and never needs root.

Installer flow (same SSH key and login as `install`):

```sh
ssh -i <key> <user>@<host> \
  '~/devkit-game/frameplayer/current/bin/frameplayer-probe --headless --summary-only'
# exit status 0 = report written; stdout = the text summary (show it to the user)
scp -i <key> <user>@<host>:frameplayer-probe-report.json <user>@<host>:frameplayer-probe-report.txt  ./
```

* Report paths on the headset: `~/frameplayer-probe-report.json` and
  `~/frameplayer-probe-report.txt` (or `--out DIR`).
* Allow about 5 minutes for the SSH command. A normal headless run takes
  10–60 s, and each check is hard-limited (video 120 s, OpenXR 90 s, …).
* Optional `--with-session` also reads swapchain formats and refresh rates
  (P1, P12). It creates an OpenXR session, which can briefly take over the
  headset display, so only use it when asked to.
* For the full test (controllers, hands, eyes, frame timing), the owner
  launches **FramePlayer Self-Test** from the Steam library with the
  headset on (a second non-Steam shortcut whose executable is
  `frameplayer-probe` with no arguments), then the installer fetches the same
  two files with `scp`.

Suggested installer UX: print the `.txt` summary, save both files next to the
installer, and tell the owner to attach `frameplayer-probe-report.txt`
and `frameplayer-probe-report.json` (or paste them) into the issue.

## Development

```sh
CARGO_TARGET_DIR=target-probe cargo test -p fp-probe
CARGO_TARGET_DIR=target-probe cargo clippy -p fp-probe --all-targets -- -D warnings
CARGO_TARGET_DIR=target-probe cargo build -p fp-probe --release --target aarch64-unknown-linux-gnu
```

Hidden `selftest_*` checks (`--only selftest_crash` and others) exercise the
runner's crash, timeout, panic and exit handling and are used by `tests/runner.rs`.
Adding a check: write `fn run(&CheckContext) -> CheckOutput` in `src/checks/`,
register it in `checks::registry()` and give it a slot in `runner::IMPORTANCE`.
