FramePlayer — a native VR video player for the Valve Steam Frame
=================================================================

Run: ./frameplayer.sh            (Steam starts it this way)
     ./frameplayer.sh VIDEO      (open a file or URL directly)

Files
  frameplayer        the player
  frameplayer.sh     launcher (sets up lib/)
  lib/               FFmpeg 7.1.1 + dav1d 1.5.3 (LGPL / BSD), see licenses/
  assets/steam/      Steam library artwork
  VERSION            installed version

Your data
  Settings:   ~/.config/frameplayer/settings.json
  Sources:    ~/.config/frameplayer/sources.json (passwords; owner-only)
  Library:    ~/.local/share/frameplayer/
  Log:        ~/.local/share/frameplayer/frameplayer.log

Videos in ~/Videos, ~/Downloads and on microSD cards / USB drives are added
to the library automatically. Haptic scripts (.funscript) next to a video or
in ~/Interactive are picked up too.

Project page: https://github.com/yellkell/frameplayer
