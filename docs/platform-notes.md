# Steam Frame platform notes

Answers to every item marked **[verify]** in [OUTLINE.md](OUTLINE.md) §1.
Milestone 0 exits when every row below says something other than *pending*.

**Status (2026-10-02): pending the first run on a Steam Frame.** The probe
that fills this page is built and tested; no Frame was available to run it.

## How to fill this page

1. Pair a Frame in Developer Mode (Frame Control, FrameDrop, or Valve's
   Devkit Client) and wake the headset.
2. From a PC with this repository: `tools/frame-probe.sh --build`.
   Or copy `frame-probe` to the headset and run it from a Desktop Mode
   terminal. Either way it writes `~/frameplayer-probe/probe-<time>.{txt,json}`.
3. Copy each answer from the report's **Answers** block into the table, and
   commit the `.json` under `docs/probe-reports/` for the record.
4. Run it once more launched from the Steam library (add the binary as a
   non-Steam game) to capture the environment a real app gets, including any
   Steam Linux Runtime container differences.

## Answers

| Question (from OUTLINE §1) | Probe verdict | Answer | Design consequence |
|---|---|---|---|
| Native OpenXR runtime reachable from a plain ARM64 binary | *pending* | | |
| Display refresh rates a native app can request | *pending* | | Frame pacing targets; 90/120 Hz plans in Milestone 5 |
| Recommended per-eye render size | *pending* | | Projection mesh density, GPU budget (§3.4) |
| V4L2 hardware decoder usable without root | *pending* | | Primary decode path (§3.3 step 2) |
| Vulkan Video decode on the GPU driver | *pending* | | Alternative decode path |
| Zero-copy DMA-BUF import of NV12 into Vulkan | *pending* | | Whether §3.3 step 3 works as designed |
| Passthrough controllable from OpenXR | *pending* | | MR background feature, Milestone 5 |
| Eye gaze via XR_EXT_eye_gaze_interaction | *pending* | | Gaze UI, Milestone 5 |
| Hand tracking via XR_EXT_hand_tracking | *pending* | | Hands-only mode |
| Frame controller interaction profile | *pending* | | Default action bindings |
| Foveated rendering hooks | *pending* | | §3.4 GPU budget |
| Runtime equirect/cylinder layers | *pending* | | Could hand 180/360 projection to the compositor |
| Swapchain formats (sRGB / 10-bit or float) | *pending* | | HDR tone-mapping output format |
| C runtime baseline and system Vulkan loader | *pending* | | What to bundle vs take from SteamOS |

## What the probe has been validated against

Not a Frame. On an x86_64 Linux machine with Monado 21 (simulated HMD, null
compositor) and Mesa lavapipe, the native build exercised the full OpenXR
path: manifest discovery, direct runtime negotiation, instance and system
queries, controller-profile probing, and a Vulkan-backed session reporting
swapchain formats, reference spaces and refresh rates. The verdicts came out
as expected for that setup (Valve Index and simple-controller profiles
accepted; Frame profile rejected as unknown to Monado).

The ARM64 build (glibc 2.28 baseline, depends only on libc) ran under QEMU
user-mode and degraded cleanly with no runtime or GPU present.

Not exercised anywhere yet: V4L2 decoder enumeration on real hardware,
Vulkan Video capability queries (lavapipe has no video queue), and SteamVR's
runtime specifically.
