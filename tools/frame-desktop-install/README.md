# Installing from Desktop Mode

`frame-apps-install.py` installs FramePlayer and Chromium XR on a Steam Frame
from Desktop Mode, with no Developer Mode, PC or SSH, and adds them to the
Steam library. The Frame's desktop Steam has no "Add a Non-Steam Game" menu,
so the installer adds the library entry itself:

- **live**: through the Steam client's DevTools port (127.0.0.1:8080), as
  saphid's installer does. Steam shows the entry at once.
- **file**: by editing `userdata/<account>/config/shortcuts.vdf`, as
  `frameplayer-install` does over SSH. Steam is closed first (it asks), and
  the entry appears back in Gaming Mode.

`--steam auto` (the default) tries live, then file. The entry, app id, `VR` tag
and artwork names match `crates/frameplayer-install/src/shortcut.rs`.

```sh
python3 frame-apps-install.py                          # both apps, latest releases
python3 frame-apps-install.py --app frameplayer
python3 frame-apps-install.py --dir ~/Apps --name-suffix " (test)"
python3 frame-apps-install.py --remove --app chromium-xr --dir ~/Apps --name-suffix " (test)"
```

To run it without a terminal, put a `.desktop` launcher next to it, e.g.
`Exec=konsole --hold -e python3 /home/steamos/Apps/frame-apps-install.py`;
Desktop Mode asks once whether to trust it.

`test_frame_apps_install.py` covers the binary VDF round trip, the shortcut
entry and the zip unpacking (`python3 -m unittest` in this folder).

Open questions, to settle on a headset with Developer Mode off:
- whether Steam's DevTools port is open without Developer Mode;
- whether a library entry made this way opens in VR like one made over SSH.
