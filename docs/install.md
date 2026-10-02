# Installing FramePlayer on your Steam Frame

Most people are done in under two minutes. You need the headset, a computer
on the same Wi-Fi, and (the first time only) about five minutes of setup.

## Fastest: one click from the website (Tier 1)

### One-time setup

1. **Turn on Developer Mode on the headset.**
   Settings → System → Developer Mode → On. The headset may restart.
2. **Install a pairing tool on your computer** (both are free):
   - **Frame Control**, or
   - **FrameDrop**.
3. **Pair it with the headset.** Open the tool, pick your Frame from the
   list, and press **Allow** when the headset asks. No passwords.

### Install

4. Open the FramePlayer download page and click
   **Install with Frame Control** or **Install with FrameDrop**
   (whichever tool you paired).
5. Your browser asks to open the tool: allow it. The tool downloads
   FramePlayer, checks its fingerprint (SHA-256), and copies it to the headset.
6. Put the headset on: **Library → Non-Steam → FramePlayer**, with its own
   artwork. Press play.

That's it. Steps 4–6 take well under two minutes on a normal connection.

### Updates

FramePlayer updates itself. When a new version is out you'll see a prompt in
the app; it downloads in the background (often only the changed parts) and
switches over the next time you start it. If a new version ever fails to
start, FramePlayer automatically goes back to the previous one after a few
tries, so you're never left with a broken install.

## Alternative: the FramePlayer installer (Tier 2)

For people who prefer a single small program, or whose pairing tool isn't
working. Works on Windows 10/11, macOS and Linux.

1. Turn on Developer Mode on the headset (as above).
2. Download `frameplayer-install` for your computer from the
   [latest release](https://github.com/yellkell/frameplayer/releases/latest).
   On macOS/Linux, make it executable: `chmod +x frameplayer-install-*`.
3. Pair (once):

   ```
   frameplayer-install pair
   ```

   It finds the headset on your network and asks you to press **Allow** on
   the headset. If it can't find it, look up the headset's IP address
   (Settings → Wi-Fi → your network) and run
   `frameplayer-install pair --host 192.168.x.y`.
4. Install:

   ```
   frameplayer-install install
   ```

   Add `--pin` to also put FramePlayer in your Favorites.

Other commands: `frameplayer-install launch`, `status`, `logs`,
`uninstall` (add `--purge` to also delete your settings and library database).

Windows needs the built-in **OpenSSH Client** (Settings → System → Optional
features → OpenSSH Client); it's already there on most Windows 10/11 PCs.

## Troubleshooting

| Problem | Fix |
|---|---|
| The tool can't find the headset | Same Wi-Fi network? Developer Mode on? Some routers block device discovery: use the headset's IP address (`--host`). |
| "pairing was declined" | Run pair again and press **Allow** on the headset within three minutes. |
| FramePlayer isn't in the library | Restart Steam on the headset (or the headset). Still missing: run `frameplayer-install install` again; it repairs the library entry. |
| It installed but won't start | Run `frameplayer-install logs` and attach the output to a bug report. |
| I want the beta | `frameplayer-install install --channel beta`, or switch channel in FramePlayer's settings. |

## Removing FramePlayer

- With the CLI: `frameplayer-install uninstall` (keeps your settings) or
  `frameplayer-install uninstall --purge` (removes everything).
- With Frame Control / FrameDrop: use the tool's uninstall option, or delete
  the Non-Steam shortcut in the headset's library.

## What gets installed where (for the curious)

| Path on the headset | What |
|---|---|
| `~/devkit-game/frameplayer/` | the app (all versions, launcher, update state) |
| `~/.local/share/frameplayer/` | library database, thumbnails, logs |
| `~/.config/frameplayer/` | settings |
| `~/.cache/frameplayer/` | disposable cache (re-created as needed) |

Nothing outside your home directory is touched, and no system files change.
