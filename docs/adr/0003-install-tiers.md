# ADR 0003: Installation tiers, one tarball, signed manifests

- Status: accepted
- Date: 2026-10-02
- Context: OUTLINE §4 (tiers), §2.2 (self-update), §3.6 (security), §5 (release)

## Decision

Ship three installation tiers that all deliver **the same release tarball**:

| Tier | Who installs | Mechanism | Updates |
|---|---|---|---|
| 1 | Frame Control / FrameDrop (community tools, already paired) | website button → `frame-control://install?manifest=<url>` / FrameDrop link → tool downloads tarball, checks SHA-256, extracts flat into `~/devkit-game/frameplayer`, registers shortcut + artwork | in-app updater |
| 2 | `frameplayer-install` (our CLI, macOS/Windows/Linux) | Valve devkit pairing (mDNS `_steamos-devkit._tcp`, `POST /register` on port 32000, approve on headset) → system `scp` upload → remote `tar` → Steam shortcut + artwork via Steam's CEF DevTools over an SSH tunnel | in-app updater |
| 3 | Steam Store | Linux ARM64 depot | Steam |

### Tarball layout (`frameplayer-<v>-aarch64.tar.gz`)

```
frameplayer.sh                 launcher (Steam runs this)
RELEASE                        "<v>"
versions/<v>/bin/frameplayer   the binary
versions/<v>/lib/              bundled shared libs (normally empty)
versions/<v>/share/frameplayer/ assets (meshes, fonts, HRTF)
versions/<v>/share/steam/{grid,grid_horizontal,hero,logo,icon}.png
```

A flat extraction is a valid install. On start, `frameplayer.sh` sees that
`RELEASE` names a version it has not adopted yet (`.installed-release`) and
switches the `current` symlink to it. The in-app updater
(`fp_updater::layout`) uses the same files, so tiers can be mixed freely:
install with Frame Control, update in-app, reinstall with the CLI.

### In-app update and rollback

- Channel manifests `updates/{stable,beta}.json` on GitHub Pages, each with a
  detached ed25519 signature (`.sig`, base64). Trusted public keys are
  compiled in from `crates/updater/release-public-keys.txt`; the secret key
  lives only in the `FP_SIGNING_KEY` Actions secret. The client verifies the
  raw bytes before parsing.
- Payloads are verified by SHA-256 (compressed tarball, and the uncompressed
  tar for deltas). Downloads resume with HTTP Range.
- Delta updates: `FPDELTA1`, an rsync-style block delta between uncompressed
  tars (rolling weak hash, byte-verified matches, gzip op stream, whole-file
  SHA-256 of source and target in the header). The client keeps the running
  version's tar in `cache/` as the delta base; if anything fails it falls back
  to the full tarball.
- Install: extract into `staging/` (same filesystem), move to
  `versions/<v>`, then `rename(2)` a temp symlink over `current`; the old
  version becomes `previous`.
- Health check: the new version is on trial (`trial` = `<v> <attempts>
  <max>`). The launcher counts launches; the app marks itself healthy after
  running N seconds (`LaunchGuard`). After `max` (3) unhealthy launches the
  launcher (or the app's `boot_check`) points `current` back at `previous` and
  lists the version in `blocked` so it is not offered again.

### Website manifest (`dist/frameplayer.json`)

Neither Frame Control nor FrameDrop publishes a manifest schema [verify].
Ours is a superset with structured fields plus flat aliases holding identical
values, validated by `fp_installer::site_manifest` and described by
`dist/frameplayer.schema.json`:

| Field | Meaning |
|---|---|
| `manifest_version` | `1` |
| `id`, `name`, `version`, `description`, `author`, `homepage`, `license` | metadata; `name` is the library display name |
| `platform`, `arch` | `linux`, `aarch64` |
| `install_dir` | directory under `~/devkit-game/` (`frameplayer`) |
| `tarball.{url,sha256,size,format}` | the release tarball |
| `launch.{command,args,working_dir,env}` | `./frameplayer.sh`, relative to the install dir |
| `artwork.{grid,grid_horizontal,hero,logo,icon}.{url,path}` | Steam artwork as a URL and as a path inside the installed tree |
| `min_steamos` | optional minimum `VERSION_ID` |
| `update_manifest` | URL of the signed in-app update channel |
| `release_notes_url`, `published` | |
| `url`, `download_url`, `sha256`, `size` | aliases of `tarball.*` |
| `launch_command`, `executable` | aliases of `launch.command` (with/without `./`) |

When the tools' expected field names are confirmed, add any missing alias to
`SiteManifest` and the schema; existing fields stay for compatibility.

### Release pipeline

`git tag vX.Y.Z` → `.github/workflows/release.yml`: build image (SLR 4 arm64
SDK, fallback Debian bookworm) → `tools/release.sh` builds the binary, the
deterministic tarball, `.sha256`, delta from the previous tag, both channel
manifests (stable releases also refresh beta) and signs them
(`frameplayer-install sign-manifest --require-trusted`) → GitHub Release with
tarball, patches and desktop installers for Linux/macOS/Windows → GitHub
Pages: download page, `frameplayer.json`, `updates/*.json(.sig)`.

## Consequences

- Tier 1 depends on third-party tools' manifest handling and on Valve keeping
  sideloading open; Tier 2 is under our control; Tier 3 removes the problem.
- The launcher is shell, not Rust, so it can recover from a binary that does
  not start at all; `crates/updater/tests/launcher.rs` keeps the two
  implementations in sync.
- Losing `FP_SIGNING_KEY` means shipping one release (signed by the old key)
  that trusts a new key; losing control of it means revoking by shipping a
  key list without it. Keys are listed one per line for this reason.
- Groundwork for Tier 3: the binary already lives in a self-contained
  directory with relative paths, needs nothing outside it but glibc and the
  system Vulkan loader, and the updater disables itself when not running from
  a managed layout (`InstallLayout::from_env()` returns `None`), which is
  what a Steam depot install looks like.
