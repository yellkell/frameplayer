# FramePlayer user guide

FramePlayer plays flat, 180°, 360°, fisheye and YouTube-style (EAC) videos,
2D or 3D, on the Steam Frame, from the headset, a microSD card, a USB drive
or your network.

## Install

You need a Steam Frame with **Developer Mode** on, paired once with your PC
by `frameplayer-install pair` (below), Frame Control, FrameDrop or Valve's
SteamOS Devkit Client.

**One click (Frame Control / FrameDrop).** Open the install link from the
release page. It points the tool at `framedrop.json`, which downloads the
release, checks its SHA-256 and unpacks it into `~/frameplayer`.

**From a PC with the installer.** Download `frameplayer-install` for your
PC from the release page and run it:

```
frameplayer-install pair 192.168.0.68       # once: the headset's IP address
frameplayer-install                         # latest release
frameplayer-install --zip frameplayer-0.1.0-aarch64.zip
frameplayer-install status | uninstall
```

To pair, open Steam Settings › Developer › Pair new host on the headset
and keep that screen showing while `pair` runs. It creates an RSA key
(`~/.ssh/id_rsa_frame_devkit`; the headset does not accept ed25519 keys),
registers it with the headset and adds a `Host frame` entry to
`~/.ssh/config`. If you skip this step, `frameplayer-install` offers to
pair when it cannot log in.

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
| microSD card / USB drive (exFAT from a PC, or ext4) | Added automatically when inserted (Settings › Library) |
| Other folders on the headset | Settings › Library › Add folder |
| XBVR, Stash and other DeoVR / HereSphere servers | Sources › Add source › DeoVR feed or HereSphere API |
| NAS / Windows share | Sources › Add source › SMB share (`smb://nas/videos`) |
| Nextcloud and other WebDAV | Sources › Add source › WebDAV |
| Plex, Jellyfin, Emby, Serviio | Sources › Add source › DLNA › Search the network |
| A web server folder listing | Sources › Add source › HTTP |

### Locking a source with a PIN

To keep a share or folder private when someone else uses the headset, press
the padlock next to it under **Sources** and choose a PIN (4 to 8 digits).
While it is locked it can't be browsed, and its videos are left out of the
library, continue watching, history, search and the phone remote; opening
one asks for the PIN. **Unlock** opens it until FramePlayer restarts; the
padlock then locks it again at once, changes the PIN or removes it.
Removing the source also removes its PIN. This keeps guests out, it doesn't
encrypt anything: the files themselves stay as they are.

To lock one folder of a NAS, add that folder as its own source
(`smb://nas/videos/private`) and lock it. Everything in it is then hidden
everywhere, also when browsing a source for the whole share, and from the
library even if it was added through that source.

### microSD cards and USB drives (VR180 and everything else)

FramePlayer notices when a card or drive is inserted (a "Found drive"
message appears), adds its videos to the library and makes thumbnails. You
can also browse it directly under **Sources › microSD and USB drives**.

**Copying videos from a Windows PC or Mac:** format the card as **exFAT**
on the computer (Windows: right-click the card › Format › exFAT), copy the
videos onto it and put it in the Frame. exFAT has no 4 GB file limit, so
8K files are fine; FAT32 cannot hold files over 4 GB.

- The Frame's SteamOS only mounts ext4 cards itself (the kind Steam
  formats for games) and no USB drives. FramePlayer mounts exFAT, FAT32
  and NTFS cards, and USB drives, itself while it is running, under
  `/run/media/steamos/<name>`.
- Steam may say the card needs formatting. **Don't format it from Steam**:
  that erases the card and makes it ext4, which Windows and macOS can't
  read. FramePlayer still finds the videos.
- An ext4 card formatted by Steam works too (shared with games: Steam game
  folders, `steamapps`, are skipped), but only the Frame can read it.
- When the card is removed its videos stay in the library with your
  ratings, resume points and adjustments; they play again once it is back.
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

Movie shapes (16:9, 4:3, 1.85, scope) play on a flat screen, and full
side-by-side or over/under 3D movies (3840x1080, 1920x2160) as flat 3D. A
video nothing identifies starts as 180° side by side; Settings › Playback ›
**Default format** picks another.

To change it: open the video, press **⚙** on the control bar, **Format**.
Overrides are remembered per video. The same panel has position, zoom,
IPD/depth, stereo alignment, picture controls, subtitle depth and
keyframes (adjustments that change smoothly along the video).

Codecs: H.264, HEVC (8/10-bit, HDR10/HLG tone-mapped), AV1, VP9, VP8,
VC-1, MPEG-4, MPEG-2, ProRes. Audio: AAC, AC-3/E-AC-3, Opus, Vorbis, FLAC,
MP3, DTS, TrueHD, PCM (Blu-ray and DVD LPCM too), and spatial (ambisonic)
audio that follows your head.
Subtitles: SRT, ASS/SSA, WebVTT, and embedded text or picture subtitles.

### Videos that stutter: 10-bit HEVC and AV1

The Frame's hardware decoder plays 8-bit 4:2:0 H.264 and HEVC (H.265), up
to 8K. 10-bit HEVC, 4:2:2 or 4:4:4 colour and AV1 have no hardware path on
the Frame, so they decode on the CPU, which can't keep up with VR
resolutions: they stutter or stop to buffer, and the control bar says why
("No hardware decoding for 10-bit HEVC", "… for 4:2:2 HEVC"). Convert them
to 8-bit 4:2:0 HEVC once on a PC and they play smoothly.

"Hardware decoder busy" means the file is fine but the decoder had no room
for it (Steam's own streaming uses it too). "The hardware decoder can't
play this HEVC" means it tried and refused the file; frameplayer.log says
why.

To check a file: `ffprobe -v error -select_streams v:0 -show_entries
stream=codec_name,profile,pix_fmt video.mp4` (`Main 10` / `yuv420p10le`
is 10-bit, `yuv422p` is 4:2:2), or MediaInfo's *Bit depth* and *Chroma
subsampling*. Windows' file properties don't tell these reliably.

With an NVIDIA graphics card (fast):

```
ffmpeg -hwaccel cuda -i video.mp4 -map 0:v:0 -map 0:a? -c:v hevc_nvenc -preset p5 -profile:v main -pix_fmt yuv420p -rc vbr -cq 20 -b:v 0 -tag:v hvc1 -c:a copy -movflags +faststart video_8bit.mp4
```

Any PC (slower; `-crf` lower is better quality and bigger):

```
ffmpeg -i video.mp4 -map 0:v:0 -map 0:a? -c:v libx265 -preset fast -crf 20 -profile:v main -pix_fmt yuv420p -tag:v hvc1 -c:a copy -movflags +faststart video_8bit.mp4
```

Or in HandBrake: the **H.265** encoder for your graphics card (NVEnc, AMD
VCE or Intel QSV, not one marked 10-bit), Constant Quality about 20,
framerate Same as source, audio Passthru, and **Dimensions › Cropping:
None** (automatic cropping cuts into side-by-side VR pictures).

Keep the format tags in the name (`_180_LR` and so on); conversions drop
the VR metadata, so a file without tags may need its format set once
under ⚙ › Format.

## Controls

Each button does one thing: the right hand runs playback, the left hand the
picture. Play/pause and seeking are under either thumb.

| Control | Right hand | Left hand |
|---|---|---|
| Thumbstick left / right | Seek back / forward 10 s (adjustable; hold to repeat). Pointing at a list: previous / next page | The same |
| Thumbstick up / down | Volume (or scroll a menu you point at) | Tilt the picture (or scroll a menu you point at) |
| Thumbstick press | Mute | Reset the picture |
| A / D-pad down | Play / pause | Play / pause |
| B / D-pad right | Back: closes the open panel, then shows the library over the video, then returns to the video | Passthrough on / off (in the library and with flat videos) |
| X / D-pad left | Previous video in the list you opened it from | Adjust panel |
| Y / D-pad up | Next video | Show or hide the controls |
| Menu / View | Library | Recenter |
| Bumper | Forward 1 minute | Back 1 minute |
| Grip + thumbstick | Up / down raises or lowers you | Left / right turns the picture, up / down zooms in / out |
| Hold grip + trigger, move | Drag the picture (the dome follows your hand) | The same |
| Trigger | Click; on empty space, show or hide the controls | The same |

Both grips together also recenter. Point at the library or adjust panel (or
the keyboard under it) and hold grip to move it; it stays where you leave
it. Recentering brings them back in front of you, at the height you left
them. The format on the control bar (for example 180° SBS) opens the adjust
panel's Format tab.

Every button and thumbstick direction above can be changed in Settings ›
Controller (tap a row, pick an action; Reset to defaults puts this layout
back). For example, to zoom with the right thumbstick, set the right controller's
thumbstick **↑↓ Up / down** (or **Grip + ↑↓**) to Zoom. The trigger,
grip + trigger drags, both grips and menu paging are fixed.

Drags, turns, tilts and zoom count as adjustments: save them for that video
under ⚙, or press the left thumbstick to undo them.

Text fields bring up a keyboard below the panel you type in; **#+=**
switches it to symbols (`! # $ % ( ) [ ] { }` and the rest) and **abc** back. You can also
type on your phone through the web remote.

## Passthrough videos

The adjust panel's **Passthrough** tab (the sliders button on the control
bar, then the glasses tab) makes part of a video see-through so your room
shows behind it. The glasses button on the bar itself only turns passthrough
on and off.

Passthrough videos are a one-time $4.99 unlock. Press **Unlock** in either
Passthrough tab, then on your phone or computer go to yellkell.com/unlock and
enter the code FramePlayer shows. It unlocks a few seconds after you pay. To
unlock again on another headset or after a reinstall, enter the new code and
restore with the email you paid with. Showing your room around flat videos and
the menus is free.

### Videos with their own mask (`_alpha`)

Many passthrough videos carry a see-through mask inside the
picture, and their names end in `_alpha` (for example
`Scene_4096p_FISHEYE190_alpha.mp4`). FramePlayer reads the mask by itself:
only the people stay and the room shows around them, with nothing to set.
**Use the video's mask** in the Passthrough tab turns it on or off for a
file, for example one renamed without `_alpha`. Only side-by-side videos
can carry a mask.

### Green and blue screens (chroma key)

Videos filmed against a green or blue screen can show your room instead of
the screen. Turn on **Remove the background** and pick the colour (green
screen, blue screen, or any colour with the red/green/blue sliders).
**Similarity** sets how close a colour must be to disappear, **Edge softness**
feathers the cut, and **Spill removal** takes the screen's tint off hair and
edges. A keyed video turns passthrough on by itself, whatever its format.

**Settings > Passthrough** sets this for every video. To change one video,
open the adjust panel's **Passthrough** tab while it plays: changing anything
there gives that video its own settings (save them like any other
adjustment), and turning **Own settings for this video** off makes it follow
Settings again.

## Web XR games and experiences

WebXR pages run in **Chromium XR for Steam Frame**, a separate app from this
repository's releases (`ChromiumXR-Frame-arm64.zip`, unpacked into
`~/chromium-xr-frame` with its Steam library entry; see
[docs/webxr](webxr/README.md)). Start it from your library: it opens Fire
Fight 2 (`https://ff2.web.app`) at 90 Hz, both eyes render, and the Frame
controllers work like Quest Touch controllers in Quest-made games. Its
address bar goes anywhere else; to change the page it starts on, put an
address in `~/.config/chromium-xr-frame/home-url`.

Notes: Chromium XR currently runs with part of Chromium's sandbox switched
off (its `--disable-seccomp-filter-sandbox` flag), because SteamVR refuses
the sandboxed browser otherwise. Use it for VR pages you trust.
[docs/webxr](webxr/README.md) has the patches that fix this properly.

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

- Log: `~/.local/share/frameplayer/frameplayer.log`; the run before is kept
  as `frameplayer.log.old` (the one to send after a freeze or crash, once
  FramePlayer has been started again)
- Settings: `~/.config/frameplayer/settings.json` (delete to reset)
- Network source passwords: `~/.config/frameplayer/sources.json`
  (readable only by you)
- "Is SteamVR running?": FramePlayer needs the headset's OpenXR runtime;
  start it from the Steam library, not a desktop terminal.
