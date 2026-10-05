#!/usr/bin/env python3
"""Installs FramePlayer and/or Chromium XR on a Steam Frame from Desktop Mode
and adds them to the Steam library. No Developer Mode, PC or SSH needed.

Runs on the headset as the normal user, with the Python 3 that SteamOS ships
(standard library only):

  python3 frame-apps-install.py                     # both apps, latest releases
  python3 frame-apps-install.py --app frameplayer
  python3 frame-apps-install.py --zip ~/Downloads/frameplayer-0.2.0-alpha.1-aarch64.zip
  python3 frame-apps-install.py --dir ~/Apps --name-suffix " (test)"
  python3 frame-apps-install.py --remove --app chromium-xr

Steps, per app: find the newest GitHub release, download its zip and check the
SHA-256 GitHub recorded for it, unpack it next to the install directory and
swap it in (the previous version is kept as <dir>.old), then add or update the
Steam library entry with its artwork.

The library entry is added one of two ways (--steam):
  live  through the Steam client's local DevTools port (127.0.0.1:8080), the
        way saphid's installer does it: Steam shows it at once.
  file  by editing userdata/<account>/config/shortcuts.vdf, the way
        frameplayer-install does over SSH. Steam must not be running while
        the file changes, so it is asked to close first (with your OK), and
        the entry appears when Steam starts again (back in Gaming Mode).
  auto  (default) live, else file.

The entry matches frameplayer-install's (crates/frameplayer-install/src/
shortcut.rs): app id crc32('"exe"' + name) | 0x80000000, the "VR" tag, and the
release's assets/steam/ artwork copied to config/grid/.
"""

import argparse
import base64
import hashlib
import json
import os
import re
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import time
import urllib.request
import zipfile
import zlib

REPO = 'yellkell/frameplayer'
API = f'https://api.github.com/repos/{REPO}/releases?per_page=30'
DEVTOOLS = 'http://127.0.0.1:8080/json'
HOME = os.path.expanduser('~')
STEAM_ROOTS = [os.path.join(HOME, '.local/share/Steam'), os.path.join(HOME, '.steam/steam')]

# What each app is, where its releases are, and how its zip is laid out.
APPS = {
    'frameplayer': {
        'name': 'FramePlayer',
        'tag': re.compile(r'^v\d'),
        'asset': re.compile(r'^frameplayer-.*-aarch64\.zip$'),
        'dir': 'frameplayer',
        'exe': 'frameplayer.sh',
        'icons': ['assets/steam/icon.png'],
        'art': 'assets/steam',
        'process': 'frameplayer/frameplayer',
    },
    'chromium-xr': {
        'name': 'Chromium XR',
        'tag': re.compile(r'^chromium-xr-frame-'),
        'asset': re.compile(r'^ChromiumXR-Frame-arm64\.zip$'),
        'dir': 'chromium-xr-frame',
        'exe': 'chromium-xr.sh',
        'icons': ['assets/steam/icon.png', 'chromium/product_logo_256.png'],
        'art': 'assets/steam',
        # Builds before test build 5 ship no Steam artwork: fetch it.
        'art_url': 'https://yellkell.com/frameapps/art/chromium-xr/',
        'process': 'chromium/chrome --user',
    },
}

ART_FILES = ['portrait.png', 'hero.png', 'logo.png', 'capsule.png', 'icon.png']

# (file in the release's art folder, file in Steam's config/grid/, DevTools asset type)
GRID_ART = [
    ('portrait.png', '{}p.png', 0),
    ('hero.png', '{}_hero.png', 1),
    ('logo.png', '{}_logo.png', 2),
    ('capsule.png', '{}.png', 3),
]
TAG = 'VR'


class Fail(Exception):
    """A problem to report to the user in one line."""


def say(msg=''):
    print(msg, flush=True)


def ask(question):
    """Yes/no question: a KDE dialog when there is a desktop, else the terminal."""
    if os.environ.get('FRAME_APPS_YES') == '1':
        return True
    if os.environ.get('DISPLAY') or os.environ.get('WAYLAND_DISPLAY'):
        for cmd in (['kdialog', '--title', 'Frame Tools', '--yesno', question],
                    ['zenity', '--question', '--title=Frame Tools', f'--text={question}']):
            if shutil.which(cmd[0]):
                return subprocess.run(cmd).returncode == 0
    if sys.stdin.isatty():
        return input(f'{question} [y/N] ').strip().lower() in ('y', 'yes')
    return False


# ---------------------------------------------------------------- releases

def latest_release(app):
    with urllib.request.urlopen(urllib.request.Request(
            API, headers={'Accept': 'application/vnd.github+json'}), timeout=30) as r:
        releases = json.load(r)
    ours = [x for x in releases if not x.get('draft') and app['tag'].match(x['tag_name'])]
    if not ours:
        raise Fail(f"No {app['name']} release found on GitHub.")
    rel = max(ours, key=lambda x: x['published_at'])
    asset = next((a for a in rel['assets'] if app['asset'].match(a['name'])), None)
    if not asset:
        raise Fail(f"{app['name']} {rel['tag_name']} has no download for the Frame.")
    digest = (asset.get('digest') or '').removeprefix('sha256:') or None
    return rel['tag_name'], asset['name'], asset['browser_download_url'], asset['size'], digest


def download(url, dest, size):
    tmp = dest + '.part'
    with urllib.request.urlopen(url, timeout=60) as r, open(tmp, 'wb') as f:
        done, last = 0, 0
        while chunk := r.read(1 << 20):
            f.write(chunk)
            done += len(chunk)
            if size and (done - last >= size // 5 or done == size):
                last = done
                say(f'  {done * 100 // size:3d}%  {done / 1048576:6.1f} of {size / 1048576:.1f} MB')
    os.replace(tmp, dest)


def sha256(path):
    h = hashlib.sha256()
    with open(path, 'rb') as f:
        while chunk := f.read(1 << 20):
            h.update(chunk)
    return h.hexdigest()


# ---------------------------------------------------------------- unpacking

def extract(zip_path, dest):
    """Unpacks zip_path into dest (created), keeping Unix permissions (Python's
    zipfile drops them) and dropping a single top-level folder if the zip has
    one. Refuses entries that would land outside dest."""
    with zipfile.ZipFile(zip_path) as z:
        infos = [i for i in z.infolist() if i.filename.strip('/')]
        tops = {i.filename.split('/', 1)[0] for i in infos}
        strip = ''
        if len(tops) == 1 and all('/' in i.filename for i in infos):
            strip = tops.pop() + '/'
        root = os.path.realpath(dest)
        os.makedirs(root)
        for info in infos:
            rel = info.filename[len(strip):] if strip else info.filename
            if not rel:
                continue
            target = os.path.realpath(os.path.join(root, rel))
            if target != root and not target.startswith(root + os.sep):
                raise Fail(f'The download contains an unsafe path: {info.filename}')
            mode = (info.external_attr >> 16) & 0o7777
            if info.is_dir():
                os.makedirs(target, exist_ok=True)
                continue
            os.makedirs(os.path.dirname(target), exist_ok=True)
            with z.open(info) as src, open(target, 'wb') as out:
                shutil.copyfileobj(src, out, 1 << 20)
            if mode:
                os.chmod(target, mode)


def fetch_art(app, dest):
    """Downloads the app's Steam artwork into its art folder when the release
    has none. Best effort: the app works without it."""
    art = os.path.join(dest, app['art'])
    if not app.get('art_url') or os.path.isfile(os.path.join(art, 'portrait.png')):
        return
    os.makedirs(art, exist_ok=True)
    got = 0
    for name in ART_FILES:
        try:
            with urllib.request.urlopen(app['art_url'] + name, timeout=30) as r:
                data = r.read()
            if data.startswith(b'\x89PNG'):
                with open(os.path.join(art, name), 'wb') as f:
                    f.write(data)
                got += 1
        except OSError:
            pass
    if got:
        say(f'  Library artwork: {got} images.')


def running(app, dest):
    pattern = os.path.join(dest, app['process'])
    return subprocess.run(['pgrep', '-f', pattern], capture_output=True).returncode == 0


def install_files(app, zip_path, dest):
    """Unpacks into <dest>.new, checks it, then swaps it in; the previous
    version stays as <dest>.old."""
    new, old = dest + '.new', dest + '.old'
    shutil.rmtree(new, ignore_errors=True)
    extract(zip_path, new)
    exe = os.path.join(new, app['exe'])
    if not os.path.isfile(exe):
        shutil.rmtree(new, ignore_errors=True)
        raise Fail(f"The download doesn't contain {app['exe']}; nothing was changed.")
    os.chmod(exe, os.stat(exe).st_mode | 0o755)
    if os.path.exists(dest):
        shutil.rmtree(old, ignore_errors=True)
        os.rename(dest, old)
    os.rename(new, dest)


# ---------------------------------------------------------------- binary VDF
# Same format as crates/frameplayer-install/src/vdf.rs. Maps are lists of
# [key, value] so order and duplicates survive; unknown scalar types are kept
# as raw bytes; anything else is refused rather than guessed at.

T_MAP, T_STR, T_INT, T_FLOAT, T_U64, T_END = 0, 1, 2, 3, 7, 8


def vdf_parse(data):
    if not data:
        return []
    pos = 0

    def cstr():
        nonlocal pos
        end = data.index(b'\0', pos)
        s = data[pos:end].decode('utf-8', 'surrogateescape')
        pos = end + 1
        return s

    def read_map(depth):
        nonlocal pos
        if depth > 32:
            raise Fail('shortcuts.vdf is nested too deeply; not editing it.')
        out = []
        while True:
            if pos >= len(data):
                if depth == 0:
                    return out
                raise Fail('shortcuts.vdf ends early; not editing it.')
            t = data[pos]
            pos += 1
            if t == T_END:
                return out
            key = cstr()
            if t == T_MAP:
                out.append([key, read_map(depth + 1)])
            elif t == T_STR:
                out.append([key, cstr()])
            elif t == T_INT:
                out.append([key, struct.unpack_from('<i', data, pos)[0]])
                pos += 4
            elif t in (T_FLOAT, T_U64):
                n = 4 if t == T_FLOAT else 8
                out.append([key, ('raw', t, data[pos:pos + n])])
                pos += n
            else:
                raise Fail(f'shortcuts.vdf has an unknown value type {t}; not editing it.')

    doc = read_map(0)
    if pos != len(data) and data[pos:] != b'\x08':
        raise Fail('shortcuts.vdf has trailing data; not editing it.')
    return doc


def vdf_write(doc):
    out = bytearray()

    def w_map(m):
        for key, v in m:
            k = key.encode('utf-8', 'surrogateescape') + b'\0'
            if isinstance(v, list):
                out.append(T_MAP); out.extend(k); w_map(v); out.append(T_END)
            elif isinstance(v, str):
                out.append(T_STR); out.extend(k); out.extend(v.encode('utf-8', 'surrogateescape') + b'\0')
            elif isinstance(v, int):
                out.append(T_INT); out.extend(k); out.extend(struct.pack('<i', v))
            else:
                _, t, raw = v
                out.append(t); out.extend(k); out.extend(raw)

    w_map(doc)
    out.append(T_END)
    return bytes(out)


def get(m, key):
    return next((v for k, v in m if k.lower() == key.lower()), None)


def put(m, key, value):
    for pair in m:
        if pair[0].lower() == key.lower():
            pair[1] = value
            return
    m.append([key, value])


# ---------------------------------------------------------------- shortcuts

def app_id(name, exe):
    return zlib.crc32(f'"{exe}"{name}'.encode()) | 0x80000000


def signed(n):
    return struct.unpack('<i', struct.pack('<I', n))[0]


def same_exe(stored, exe):
    return (stored or '').strip().strip('"') == exe


def upsert(doc, name, exe, start_dir, icon):
    shortcuts = get(doc, 'shortcuts')
    if shortcuts is None:
        shortcuts = []
        doc.append(['shortcuts', shortcuts])
    aid = signed(app_id(name, exe))
    entry = next((v for _, v in shortcuts if isinstance(v, list) and
                  (get(v, 'appid') == aid or same_exe(get(v, 'Exe'), exe))), None)
    added = entry is None
    if added:
        used = {k for k, _ in shortcuts}
        key = str(next(i for i in range(len(shortcuts) + 1) if str(i) not in used))
        entry = []
        shortcuts.append([key, entry])
    put(entry, 'appid', aid)
    put(entry, 'appname', name)  # Steam's own spelling
    put(entry, 'Exe', f'"{exe}"')
    put(entry, 'StartDir', f'"{start_dir}"')
    put(entry, 'icon', icon)
    put(entry, 'LaunchOptions', '')
    for key, value in [('ShortcutPath', ''), ('IsHidden', 0), ('AllowDesktopConfig', 1),
                       ('AllowOverlay', 1), ('OpenVR', 0), ('Devkit', 0), ('DevkitGameID', ''),
                       ('DevkitOverrideAppID', 0), ('LastPlayTime', 0), ('FlatpakAppID', ''),
                       ('tags', [])]:
        if added or get(entry, key) is None:
            put(entry, key, value)
    tags = get(entry, 'tags')
    if isinstance(tags, list) and TAG not in [v for _, v in tags]:
        used = {k for k, _ in tags}
        tags.append([str(next(i for i in range(len(tags) + 1) if str(i) not in used)), TAG])
    return app_id(name, exe), added


def remove_entries(doc, name, exe):
    shortcuts = get(doc, 'shortcuts') or []
    keep = [[k, v] for k, v in shortcuts if not (isinstance(v, list) and (
        get(v, 'AppName') == name or same_exe(get(v, 'Exe'), exe)))]
    if len(keep) == len(shortcuts):
        return False
    shortcuts[:] = [[str(i), v] for i, (_, v) in enumerate(keep)]
    return True


def steam_accounts():
    """config/ folders of the Steam accounts signed in on this headset."""
    seen, out = set(), []
    for root in STEAM_ROOTS:
        base = os.path.join(root, 'userdata')
        if not os.path.isdir(base):
            continue
        for user in sorted(os.listdir(base)):
            cfg = os.path.realpath(os.path.join(base, user, 'config'))
            if user.isdigit() and user != '0' and os.path.isdir(cfg) and cfg not in seen:
                seen.add(cfg)
                out.append(cfg)
    return out


def steam_running():
    return subprocess.run(['pgrep', '-x', 'steam'], capture_output=True).returncode == 0


def close_steam():
    if not steam_running():
        return True
    if not ask('Steam needs to close for a moment so the app can be added to your library.\n'
               'It starts again when you return to Gaming Mode. Close Steam now?'):
        return False
    subprocess.Popen(['steam', '-shutdown'], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                     start_new_session=True)
    for _ in range(60):
        time.sleep(1)
        if not steam_running():
            time.sleep(1)  # let it finish writing its own files
            return True
    raise Fail("Steam didn't close. Close it from its menu and run the installer again.")


def write_shortcut_file(cfg, edit):
    path = os.path.join(cfg, 'shortcuts.vdf')
    try:
        with open(path, 'rb') as f:
            data = f.read()
    except FileNotFoundError:
        data = b''
    doc = vdf_parse(data)
    result = edit(doc)
    if data:
        shutil.copy2(path, f'{path}.bak-{time.strftime("%Y%m%d-%H%M%S")}')
    fd, tmp = tempfile.mkstemp(dir=cfg, prefix='.shortcuts.')
    with os.fdopen(fd, 'wb') as f:
        f.write(vdf_write(doc))
    os.replace(tmp, path)
    return result


def copy_art(cfg, art_dir, aid):
    grid = os.path.join(cfg, 'grid')
    copied = 0
    for src, pattern, _ in GRID_ART:
        p = os.path.join(art_dir, src)
        if os.path.isfile(p):
            os.makedirs(grid, exist_ok=True)
            shutil.copyfile(p, os.path.join(grid, pattern.format(aid)))
            copied += 1
    return copied


# ------------------------------------------------- Steam DevTools ("live")

class WS:
    """Just enough RFC 6455 for CDP requests on loopback."""

    def __init__(self, url):
        host_port, path = url[len('ws://'):].split('/', 1)
        host, port = host_port.split(':')
        self.s = socket.create_connection((host, int(port)), timeout=20)
        key = base64.b64encode(os.urandom(16)).decode()
        self.s.sendall((f'GET /{path} HTTP/1.1\r\nHost: {host_port}\r\nUpgrade: websocket\r\n'
                        f'Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\n'
                        'Sec-WebSocket-Version: 13\r\n\r\n').encode())
        buf = b''
        while b'\r\n\r\n' not in buf:
            chunk = self.s.recv(4096)
            if not chunk:
                raise EOFError('closed during handshake')
            buf += chunk
        if b' 101 ' not in buf.split(b'\r\n', 1)[0]:
            raise OSError('websocket handshake refused')
        self.rest = buf.split(b'\r\n\r\n', 1)[1]

    def _read(self, n):
        while len(self.rest) < n:
            chunk = self.s.recv(65536)
            if not chunk:
                raise EOFError
            self.rest += chunk
        out, self.rest = self.rest[:n], self.rest[n:]
        return out

    def send(self, text):
        data = text.encode()
        mask = os.urandom(4)
        n = len(data)
        head = bytes([0x81]) + (bytes([0x80 | n]) if n < 126 else
                                bytes([0x80 | 126]) + struct.pack('>H', n) if n < 65536 else
                                bytes([0x80 | 127]) + struct.pack('>Q', n))
        self.s.sendall(head + mask + bytes(b ^ mask[i % 4] for i, b in enumerate(data)))

    def recv(self):
        msg = b''
        while True:
            b0, b1 = self._read(2)
            n = b1 & 0x7f
            if n == 126:
                n = struct.unpack('>H', self._read(2))[0]
            elif n == 127:
                n = struct.unpack('>Q', self._read(8))[0]
            payload = self._read(n)
            if b0 & 0x0f == 0x9:  # ping: ignore
                continue
            msg += payload
            if b0 & 0x80:
                return msg.decode()


def steam_js(js):
    """Evaluates js in the Steam client's SharedJSContext; raises OSError when
    the DevTools port is closed."""
    with urllib.request.urlopen(DEVTOOLS, timeout=3) as r:
        targets = json.load(r)
    url = next((t['webSocketDebuggerUrl'] for t in targets if t.get('title') == 'SharedJSContext'), None)
    if not url:
        raise OSError('SharedJSContext not found')
    ws = WS(url)
    ws.send(json.dumps({'id': 1, 'method': 'Runtime.evaluate', 'params': {
        'expression': js, 'awaitPromise': True, 'returnByValue': True}}))
    while True:
        r = json.loads(ws.recv())
        if r.get('id') == 1:
            break
    res = r.get('result', {})
    if 'exceptionDetails' in res:
        raise OSError('Steam rejected the request: ' + json.dumps(res['exceptionDetails'])[:300])
    return res.get('result', {}).get('value')


def live_upsert(name, exe, start_dir, icon, art_dir):
    art = {}
    for src, _, kind in GRID_ART:
        p = os.path.join(art_dir, src)
        if os.path.isfile(p):
            with open(p, 'rb') as f:
                art[kind] = base64.b64encode(f.read()).decode()
    q = json.dumps
    appid = steam_js(f'''(async () => {{
      const exe = {q(exe)}, quoted = {q('"' + exe + '"')};
      const mine = appStore.allApps.filter(a => a.app_type === 1073741824);
      let id = (mine.find(a => {{
        const d = appStore.GetAppOverviewByAppID(a.appid);
        return a.display_name === {q(name)} || (d && (d.shortcut_exe === exe || d.shortcut_exe === quoted));
      }}) || {{}}).appid;
      if (!id) id = await SteamClient.Apps.AddShortcut({q(name)}, exe, "", "");
      SteamClient.Apps.SetShortcutName(id, {q(name)});
      SteamClient.Apps.SetShortcutExe(id, quoted);
      SteamClient.Apps.SetShortcutStartDir(id, {q('"' + start_dir + '"')});
      if ({q(icon)}) SteamClient.Apps.SetShortcutIcon(id, {q(icon)});
      const art = {q(art)};
      for (const kind of Object.keys(art)) {{
        try {{ await SteamClient.Apps.SetCustomArtworkForApp(id, art[kind], "png", Number(kind)); }} catch (e) {{}}
      }}
      return id;
    }})()''')
    if not isinstance(appid, int) or appid <= 0:
        raise OSError(f'Steam returned no app id ({appid!r})')
    return appid


def live_remove(name, exe):
    q = json.dumps
    return steam_js(f'''(() => {{
      const exe = {q(exe)}, quoted = {q('"' + exe + '"')};
      const ids = appStore.allApps.filter(a => a.app_type === 1073741824).filter(a => {{
        const d = appStore.GetAppOverviewByAppID(a.appid);
        return a.display_name === {q(name)} || (d && (d.shortcut_exe === exe || d.shortcut_exe === quoted));
      }}).map(a => a.appid);
      ids.forEach(id => SteamClient.Apps.RemoveShortcut(id));
      return ids.length;
    }})()''')


# ---------------------------------------------------------------- main

def add_to_steam(method, name, exe, start_dir, icon, art_dir):
    """Returns 'live' or 'file' (how it was added); raises Fail."""
    if method in ('auto', 'live'):
        try:
            appid = live_upsert(name, exe, start_dir, icon, art_dir)
            say(f'  Added to your Steam library (app id {appid}).')
            return 'live'
        except (OSError, EOFError, ValueError) as e:
            if method == 'live':
                raise Fail(f"Couldn't reach Steam's DevTools port: {e}")
            say(f"  Steam can't be updated live here ({type(e).__name__}); editing its library file instead.")
    accounts = steam_accounts()
    if not accounts:
        raise Fail("No Steam account found on this headset. Sign in to Steam, then run this again.")
    if not close_steam():
        raise Fail('Steam was left open, so the app was installed but not added to the library.')
    for cfg in accounts:
        aid, added = write_shortcut_file(cfg, lambda doc: upsert(doc, name, exe, start_dir, icon))
        art = copy_art(cfg, art_dir, aid)
        say(f"  {'Added to' if added else 'Updated in'} the Steam library "
            f"(account {os.path.basename(os.path.dirname(cfg))}, app id {aid}, {art} artwork images).")
    return 'file'


def main():
    ap = argparse.ArgumentParser(description=__doc__.split('\n\n')[0])
    ap.add_argument('--app', choices=['all', *APPS], default='all')
    ap.add_argument('--zip', help='install this downloaded zip instead of fetching the latest release')
    ap.add_argument('--dir', default=HOME, help='folder to install into (default: your home folder)')
    ap.add_argument('--name-suffix', default='', help='added to the library name, e.g. " (test)"')
    ap.add_argument('--steam', choices=['auto', 'live', 'file', 'none'], default='auto')
    ap.add_argument('--remove', action='store_true', help='remove the app and its library entry')
    args = ap.parse_args()

    if os.uname().machine != 'aarch64' and not os.environ.get('FRAME_APPS_ANY_ARCH'):
        raise Fail(f'This installer is for the Steam Frame; this machine is {os.uname().machine}.')

    keys = list(APPS) if args.app == 'all' else [args.app]
    if args.zip:
        if len(keys) != 1:
            match = [k for k in APPS if APPS[k]['asset'].match(os.path.basename(args.zip))]
            if len(match) != 1:
                raise Fail('With --zip, also say which app it is: --app frameplayer or --app chromium-xr.')
            keys = match
    parent = os.path.abspath(os.path.expanduser(args.dir))
    cache = os.path.join(HOME, '.cache/frame-apps')
    os.makedirs(cache, exist_ok=True)
    methods = set()

    for key in keys:
        app = APPS[key]
        name = app['name'] + args.name_suffix
        dest = os.path.join(parent, app['dir'])
        exe = os.path.join(dest, app['exe'])
        say(f'\n{name}')

        if args.remove:
            if running(app, dest):
                raise Fail(f"{name} is running. Close it, then run this again.")
            removed = False
            if args.steam in ('auto', 'live'):
                try:
                    removed = live_remove(name, exe) > 0
                    methods.add('live')
                except (OSError, EOFError, ValueError):
                    if args.steam == 'live':
                        raise
            if not removed and args.steam != 'none':
                if close_steam():
                    for cfg in steam_accounts():
                        removed |= write_shortcut_file(cfg, lambda doc: remove_entries(doc, name, exe))
                    methods.add('file')
            shutil.rmtree(dest, ignore_errors=True)
            shutil.rmtree(dest + '.old', ignore_errors=True)
            say(f"  Removed {dest}{' and its library entry' if removed else ''}.")
            continue

        if args.zip:
            zip_path = os.path.abspath(os.path.expanduser(args.zip))
            say(f'  Using {zip_path}')
            try:
                _, asset, _, _, digest = latest_release(app)
                if asset == os.path.basename(zip_path) and digest:
                    if sha256(zip_path) != digest:
                        raise Fail('That zip does not match the published release (SHA-256 differs).')
                    say('  Checked against the published release.')
            except (OSError, ValueError):
                say('  (Could not reach GitHub to check it; installing as is.)')
        else:
            tag, asset, url, size, digest = latest_release(app)
            say(f'  Latest release: {tag}')
            zip_path = os.path.join(cache, asset)
            if not (os.path.isfile(zip_path) and digest and sha256(zip_path) == digest):
                download(url, zip_path, size)
            if digest:
                if sha256(zip_path) != digest:
                    os.remove(zip_path)
                    raise Fail('The download is damaged (SHA-256 differs). Run the installer again.')
                say('  Download verified.')

        if running(app, dest):
            raise Fail(f"{name} is running. Close it, then run this again.")
        os.makedirs(parent, exist_ok=True)
        install_files(app, zip_path, dest)
        say(f'  Installed in {dest}')
        fetch_art(app, dest)

        if args.steam != 'none':
            icon = next((p for p in (os.path.join(dest, i) for i in app['icons']) if os.path.isfile(p)), '')
            methods.add(add_to_steam(args.steam, name, exe, dest, icon, os.path.join(dest, app['art'])))

    say('\nDone.')
    if 'file' in methods and os.environ.get('XDG_CURRENT_DESKTOP') and shutil.which('steamos-session-select'):
        if ask('Return to Gaming Mode now? Steam starts again there with the new library entries.'):
            subprocess.Popen(['steamos-session-select', 'gamescope'], start_new_session=True)
    elif 'file' in methods:
        say('Start Steam again (or return to Gaming Mode) to see the library entries.')


if __name__ == '__main__':
    try:
        main()
    except Fail as e:
        say(f'\n{e}')
        sys.exit(1)
    except KeyboardInterrupt:
        sys.exit(130)
