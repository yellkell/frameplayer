# FramePlayer user guide

FramePlayer plays flat, 180°, 360°, fisheye and YouTube-style (EAC) videos,
2D or 3D, on the Steam Frame, from the headset, a microSD card, a USB drive
or your network.

## Install

You need a Steam Frame with **Developer Mode** on, paired once with your PC
by Frame Control, FrameDrop or Valve's SteamOS Devkit Client.

**One click (Frame Control / FrameDrop).** Open the install link from the
release page. It points the tool at `framedrop.json`, which downloads the
release, checks its SHA-256 and unpacks it into `~/frameplayer`.

**From a PC with the installer.** Download `frameplayer-install` for your
PC from the release page and run it:

```
frameplayer-install                         # latest release
frameplayer-install --zip frameplayer-0.1.0-aarch64.zip
frameplayer-install status | uninstall
```

It reuses the `frame` SSH alias created when pairing, installs into
`~/frameplayer` (keeping the previous version as `~/frameplayer.old`) and
adds **FramePlayer** to the Steam library with artwork. Add
`--restart-steam` to make it show up right away.

**By hand.** Copy the zip to the headset, unzip it into your home folder
and add `~/frameplayer/frameplayer.sh` to Steam as a non-Steam game.

FramePlayer then updates itself from the Settings › Updates page.

## Where your videos can be

| Where | How |
|---|---|
| `~/Videos`, `~/Downloads` | Added to the library automatically |
| microSD card / USB drive | Added automatically when inserted (Settings › Library) |
| Other folders on the headset | Settings › Library › Add folder |
| XBVR, Stash and other DeoVR / HereSphere servers | Sources › Add source › DeoVR feed or HereSphere API |
| NAS / Windows share | Sources › Add source › SMB share (`smb://nas/videos`) |
| Nextcloud and other WebDAV | Sources › Add source › WebDAV |
| Plex, Jellyfin, Emby, Serviio | Sources › Add source › DLNA › Search the network |
| A web server folder listing | Sources › Add source › HTTP |

### microSD cards and USB drives (VR180 and everything else)

SteamOS mounts cards and drives under `/run/media/deck/<name>`. FramePlayer
notices when one is inserted (a "Found drive" message appears), adds its
videos to the library and makes thumbnails. You can also browse a card
directly under **Sources › microSD and USB drives**.

- Steam game folders (`steamapps`) on a shared card are skipped.
- When the card is removed its videos stay in the library with your
  ratings, resume points and adjustments; they play again once it is back.
- Use ext4 or exFAT. exFAT is readable from Windows and macOS too.
- VR180 videos are recognised from their metadata (VR180 cameras, YouTube
  VR180 downloads) or from names like `Trip_VR180.mp4`,
  `Beach_180_LR.mp4` or `clip_180x180_3dh.mp4`. If one shows up wrong, set
  the format once under ⚙ › Format; it is remembered for that file.
- Large high-bitrate 8K files play best from a fast card (A2/V30 or better).

## Video formats

The format is picked automatically from, in order: your own choice for that
file, the file's spherical/stereo metadata, the file name, and the picture's
shape. File-name tags understood include `_180`, `_360`, `_LR`/`_SBS`,
`_TB`/`_OU`, `_RL`, `_FISHEYE190`, `_MKX200`, `_MKX220`, `_VRCA220`,
`_EAC`, `_2D`, `_3D`.

To change it: open the video, press **⚙** on the control bar, **Format**.
Overrides are remembered per video. The same panel has position, zoom,
IPD/depth, stereo alignment, picture controls, subtitle depth and
keyframes (adjustments that change smoothly along the video).

Codecs: H.264, HEVC (8/10-bit, HDR10/HLG tone-mapped), AV1, VP9, VP8,
MPEG-4, MPEG-2, ProRes. Audio: AAC, AC-3/E-AC-3, Opus, Vorbis, FLAC, MP3,
DTS, TrueHD, PCM, and spatial (ambisonic) audio that follows your head.
Subtitles: SRT, ASS/SSA, WebVTT, and embedded text or picture subtitles.

## Controls

The same as DeoVR's defaults on Quest, so muscle memory carries over:

| Control | Action |
|---|---|
| A / X | Play / pause |
| B / Y | Back: closes the open panel, then shows the library over the video, then returns to the video |
| Thumbstick left / right | Seek back / forward 10 s (adjustable; hold to repeat). Pointing at a list: previous / next page |
| Thumbstick up / down | Volume (or scroll a menu you point at) |
| Thumbstick press | Reset the image (zoom and drag) |
| Hold grip + trigger, move | Drag the picture (the dome follows your hand) |
| Grip + thumbstick right / left | Next / previous video in the list you opened it from |
| Grip + thumbstick down / up | Zoom in / out |
| Trigger | Click; on empty space, show or hide the controls |
| Menu | Library |
| Both grips | Recenter |

Drags and zoom count as adjustments: save them for that video under ⚙, or
press the thumbstick to undo them.

Text fields bring up a keyboard in front of you; you can also type on your
phone through the web remote.

## Web XR games and experiences

The **Web XR** tab opens WebXR pages (Fish & Chips at
`https://yellkell.com/fac` is already in the list) in a browser that can do
immersive VR on the Frame:

1. Install **Chromium XR for Steam Frame** once: the `ChromiumXR-Frame-arm64.zip`
   release of this repository, unpacked into `~/chromium-xr-frame` (see
   `docs/webxr/README.md`). It carries the Frame fixes: both eyes render,
   the controllers work like Quest Touch controllers in Quest-made games,
   and WebXR runs at 90 Hz. saphid's build in `~/.local/bin/chromium-xr`
   also works, without those fixes. The Web XR tab shows the browser it
   found.
2. In FramePlayer, **Web XR › Launch** next to Fish & Chips. FramePlayer
   closes its VR view and the page opens; press the page's **Enter VR**
   button.
3. Close the browser to return to FramePlayer's library.

Mixed-reality pages (WebXR `immersive-ar`) also
need the headset to offer passthrough to apps: Chromium only offers AR when
SteamVR reports an alpha-blend or additive blend mode. The Web XR tab (and
Settings › About) says whether this headset does; if not, such pages report
XR as unavailable while `immersive-vr` pages still work.

Add your own pages with a name and address, or type one and press **Open
now**. Only `http://` and `https://` pages open.

Notes: Chromium XR currently runs with part of Chromium's sandbox switched
off (its `--disable-seccomp-filter-sandbox` flag), because SteamVR refuses
the sandboxed browser otherwise. Use it for VR pages you trust.
[docs/webxr](webxr/README.md) has the patches that fix this properly. The
hand-off needs FramePlayer started from Steam (it runs `frameplayer.sh`);
started any other way, FramePlayer just opens the browser and quits.

## Haptics

Scripts (`.funscript`, including multi-axis `video.surge.funscript` etc.)
are found next to the video, in `~/Interactive`, or supplied by the server.
Add a device in **Settings › Haptics**: Intiface Central (Buttplug), TCode
devices (OSR2/SR6 over USB serial, TCP or UDP), or The Handy (connection
key). The seek bar shows the script's intensity.

## Remote control

**Settings › Remote control › Phone / browser remote** shows a QR code; scan
it to control playback, browse the library and type from your phone. The
**DeoVR remote API** (port 23554) works with apps that speak DeoVR's
protocol. Both only answer devices on your local network.

## Troubleshooting

- Log: `~/.local/share/frameplayer/frameplayer.log`
- Settings: `~/.config/frameplayer/settings.json` (delete to reset)
- Network source passwords: `~/.config/frameplayer/sources.json`
  (readable only by you)
- "Is SteamVR running?": FramePlayer needs the headset's OpenXR runtime;
  start it from the Steam library, not a desktop terminal.
