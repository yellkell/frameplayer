//! The installer's steps, written against [`Remote`] so they can be tested
//! with a fake headset.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use fp_updater::install::{BINARY_NAME, LAUNCHER_NAME, STEAM_ASSETS_DIR, VERSION_FILE};
use fp_updater::{ArchiveInfo, Channel, Updater, Version, hash_file, inspect_zip, select_update};

use crate::device::{DEVKIT_SHORTCUT_TOOL, DeviceInfo, probe, resolve_install_dir};
use crate::error::{InstallError, Result};
use crate::remote::{Remote, run_checked, sh_quote};
use crate::shortcut::{
    GRID_ART, ICON_FILE, ShortcutSpec, Upsert, devkit_shortcut_command, find_shortcut,
    grid_file_name, remove_shortcut, upsert_shortcut,
};
use crate::vdf::{self, Value};

/// Default install folder, relative to the headset user's home.
pub const DEFAULT_INSTALL_DIR: &str = "frameplayer";
/// Where uploads wait on the headset, relative to home.
pub const REMOTE_STAGING: &str = ".cache/frameplayer-install";
/// Game id handed to the devkit helper (devkit titles live in
/// `~/devkit-game/<id>`).
pub const DEVKIT_GAME_ID: &str = "frameplayer";
/// Extra room required beyond the zip and its unpacked size.
const SPACE_MARGIN: u64 = 64 * 1024 * 1024;

/// How to add FramePlayer to the Steam library.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum ShortcutMethod {
    /// Valve's devkit helper if present, else edit `shortcuts.vdf`.
    #[default]
    Auto,
    /// Only Valve's devkit helper (`~/devkit-utils`).
    Devkit,
    /// Only edit `shortcuts.vdf` (and set library artwork).
    Vdf,
    /// Do not touch the Steam library.
    None,
}

/// Options for [`install_zip`].
#[derive(Debug, Clone)]
pub struct InstallOptions {
    /// Install folder (`frameplayer`, `~/x`, or absolute inside home).
    pub dir: String,
    /// Library name.
    pub app_name: String,
    /// Shortcut registration method.
    pub shortcut: ShortcutMethod,
    /// Ask Steam to shut down afterwards so it reloads its library.
    pub restart_steam: bool,
    /// Copy the files even when the same version is installed.
    pub reinstall: bool,
}

/// Options for [`uninstall`].
#[derive(Debug, Clone)]
pub struct UninstallOptions {
    /// Install folder, as for [`InstallOptions::dir`].
    pub dir: String,
    /// Library name used when installing.
    pub app_name: String,
    /// Also delete settings, library database and caches.
    pub purge: bool,
    /// Ask Steam to shut down afterwards.
    pub restart_steam: bool,
}

/// Connects, probes and checks the headset, printing what was found.
pub fn connect(remote: &mut dyn Remote, force: bool) -> Result<DeviceInfo> {
    println!("Connecting to {} ...", remote.host());
    let device = probe(remote)?;
    device.check(force)?;
    let os = if device.os_name.is_empty() {
        "unknown OS"
    } else {
        &device.os_name
    };
    println!("Connected: {os}, {} (home {}).", device.arch, device.home);
    if !device.is_steamos() {
        println!(
            "Note: this does not identify as SteamOS (ID={:?}); continuing anyway.",
            device.os_id
        );
    }
    Ok(device)
}

/// Downloads the latest release for the headset (aarch64) from the signed
/// manifest at `manifest_url`, into `cache_dir`. Signature and SHA-256 are
/// verified by `fp-updater`; a cached complete download is reused.
pub fn fetch_latest(manifest_url: &str, channel: Channel, cache_dir: &Path) -> Result<PathBuf> {
    println!("Checking for the latest FramePlayer release ({channel}) ...");
    let updater = Updater::new()?.arch("aarch64");
    let manifest = updater.fetch_manifest(manifest_url)?;
    let update =
        select_update(&manifest, &Version::new(0, 0, 0), channel, "aarch64")?.ok_or_else(|| {
            InstallError::NoRelease(format!(
                "the newest release ({} on the {} channel) is not offered on the {channel} \
                 channel; try --channel beta",
                manifest.version, manifest.channel
            ))
        })?;
    println!(
        "Downloading FramePlayer {} ({:.1} MB) ...",
        update.version,
        update.artifact.size as f64 / 1e6
    );
    let mut last_pct = u64::MAX;
    let path = updater.download(
        &update,
        cache_dir,
        |done, total| {
            let pct = done.saturating_mul(100) / total.max(1);
            if pct / 10 != last_pct / 10 || (pct == 100 && last_pct != 100) {
                println!("  {pct}%");
                last_pct = pct;
            }
        },
        &AtomicBool::new(false),
    )?;
    println!("Download verified (signature and SHA-256).");
    Ok(path)
}

/// Absolute paths on the headset for one install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemotePlan {
    /// Install folder.
    pub dir: String,
    /// Where the zip is uploaded.
    pub zip: String,
    /// Expected SHA-256 of the zip.
    pub sha256: String,
    /// Version inside the zip.
    pub version: String,
    /// Top-level folder inside the zip, if any.
    pub root: Option<String>,
}

/// The shell script that verifies, unpacks, validates and swaps in a new
/// version on the headset. Mirrors `fp_updater::install`: the new tree is
/// built in `<dir>.new`, the current one is kept as `<dir>.old` (so the
/// in-app rollback works), and nothing in `<dir>` changes until the new
/// tree has passed its checks.
///
/// The zip was already checked on this computer by
/// `fp_updater::inspect_zip` (no `..`, absolute paths or links), and its
/// SHA-256 is checked again here, so `unzip` extracts exactly those files.
pub fn install_script(p: &RemotePlan) -> String {
    let src = match &p.root {
        Some(r) => format!("\"$unpack\"/{}", sh_quote(r)),
        None => "\"$unpack\"".to_string(),
    };
    format!(
        r#"set -eu
zip={zip}; dir={dir}; want_sha={sha}; want_ver={ver}
fail() {{ echo "$*" >&2; exit 1; }}
if command -v sha256sum >/dev/null 2>&1; then
  got=$(sha256sum "$zip" | cut -d' ' -f1)
elif command -v python3 >/dev/null 2>&1; then
  got=$(python3 -c 'import hashlib,sys
h=hashlib.sha256()
with open(sys.argv[1],"rb") as f:
    for b in iter(lambda: f.read(1<<20), b""): h.update(b)
print(h.hexdigest())' "$zip")
else
  got=$want_sha
fi
[ "$got" = "$want_sha" ] || fail "the uploaded zip is damaged (SHA-256 $got)"
unpack="$dir.unpack"; new="$dir.new"; old="$dir.old"
rm -rf "$unpack" "$new"
mkdir -p "$(dirname "$dir")" "$unpack"
if command -v unzip >/dev/null 2>&1; then
  unzip -q -o "$zip" -d "$unpack" || fail "unzip could not unpack the release"
elif command -v python3 >/dev/null 2>&1; then
  python3 -c 'import sys,zipfile; zipfile.ZipFile(sys.argv[1]).extractall(sys.argv[2])' "$zip" "$unpack" || fail "python3 could not unpack the release"
else
  fail "neither unzip nor python3 is available on the headset"
fi
mv {src} "$new"
rm -rf "$unpack"
[ -f "$new/{bin}" ] || fail "the release has no {bin} program"
ver=$(head -n 1 "$new/{version_file}" | tr -d ' \r\n')
[ "$ver" = "$want_ver" ] || fail "the release says version $ver, expected $want_ver"
chmod 755 "$new/{bin}"
if [ -f "$new/{launcher}" ]; then chmod 755 "$new/{launcher}"; fi
[ -x "$new/{bin}" ] || fail "cannot make {bin} executable"
if [ -e "$dir" ]; then rm -rf "$old"; mv "$dir" "$old"; fi
if ! mv "$new" "$dir"; then
  if [ -e "$old" ] && [ ! -e "$dir" ]; then mv "$old" "$dir"; fi
  fail "could not move the new version into place"
fi
rm -f "$zip"
echo "installed=$ver"
"#,
        zip = sh_quote(&p.zip),
        dir = sh_quote(&p.dir),
        sha = sh_quote(&p.sha256),
        ver = sh_quote(&p.version),
        bin = BINARY_NAME,
        launcher = LAUNCHER_NAME,
        version_file = VERSION_FILE,
    )
}

/// Reads `<dir>/VERSION` on the headset; `None` if absent or unparsable.
pub fn remote_version(remote: &mut dyn Remote, dir: &str) -> Result<Option<Version>> {
    let out = remote.run(&format!(
        "cat {} 2>/dev/null || true",
        sh_quote(&format!("{dir}/{VERSION_FILE}"))
    ))?;
    Ok(Version::parse(out.stdout.trim()).ok())
}

/// The shortcut description for an install in `dir`.
pub fn shortcut_spec(dir: &str, app_name: &str, info: &ArchiveInfo) -> ShortcutSpec {
    let program = if info.has_launcher {
        LAUNCHER_NAME
    } else {
        BINARY_NAME
    };
    let icon_rel = format!("{STEAM_ASSETS_DIR}/{ICON_FILE}");
    let icon = if info.steam_assets.contains(&icon_rel) {
        format!("{dir}/{icon_rel}")
    } else {
        String::new()
    };
    ShortcutSpec {
        app_name: app_name.to_string(),
        exe: format!("{dir}/{program}"),
        start_dir: dir.to_string(),
        icon,
        launch_options: String::new(),
    }
}

/// Installs the release zip at `zip` onto the headset described by
/// `device`, then adds it to the Steam library.
pub fn install_zip(
    remote: &mut dyn Remote,
    device: &DeviceInfo,
    zip: &Path,
    opts: &InstallOptions,
) -> Result<Version> {
    let dir = resolve_install_dir(&device.home, &opts.dir)?;
    println!("Checking {} ...", zip.display());
    let info = inspect_zip(zip)?;
    let (size, sha256) = hash_file(zip)?;
    println!(
        "FramePlayer {} ({:.1} MB), installing to {dir}.",
        info.version,
        size as f64 / 1e6
    );

    let current = remote_version(remote, &dir)?;
    let copy_files = opts.reinstall || current.as_ref() != Some(&info.version);
    if copy_files {
        upload_and_unpack(remote, device, zip, &dir, &info, size, sha256)?;
        match &current {
            Some(old) => println!(
                "Installed FramePlayer {} (replacing {old}; the previous version is kept in {dir}.old).",
                info.version
            ),
            None => println!("Installed FramePlayer {}.", info.version),
        }
    } else {
        println!(
            "FramePlayer {} is already installed; only checking the Steam shortcut \
             (use --reinstall to copy the files again).",
            info.version
        );
    }

    let spec = shortcut_spec(&dir, &opts.app_name, &info);
    register_shortcut(remote, device, &spec, &info, opts)?;
    Ok(info.version)
}

fn upload_and_unpack(
    remote: &mut dyn Remote,
    device: &DeviceInfo,
    zip: &Path,
    dir: &str,
    info: &ArchiveInfo,
    size: u64,
    sha256: String,
) -> Result<()> {
    if !device.has_unzip && !device.has_python3 {
        return Err(InstallError::Requirement(
            "the headset has neither unzip nor python3, so the release cannot be unpacked".into(),
        ));
    }
    let needed = size
        .saturating_add(info.uncompressed_size)
        .saturating_add(SPACE_MARGIN);
    if let Some(free) = device.free_bytes {
        if free < needed {
            return Err(InstallError::Requirement(format!(
                "the headset has {:.0} MB free but the install needs about {:.0} MB; \
                 free some space and try again",
                free as f64 / 1e6,
                needed as f64 / 1e6
            )));
        }
    }
    let staging = device.home_path(REMOTE_STAGING);
    let plan = RemotePlan {
        dir: dir.to_string(),
        zip: format!("{staging}/frameplayer-{}.zip", info.version),
        sha256,
        version: info.version.to_string(),
        root: info.root.clone(),
    };
    run_checked(
        remote,
        "preparing the upload folder",
        &format!("mkdir -p {}", sh_quote(&staging)),
    )?;
    println!("Copying to the headset ...");
    remote.upload(zip, &plan.zip)?;
    println!("Unpacking on the headset ...");
    let out = run_checked(remote, "unpacking FramePlayer", &install_script(&plan))?;
    let reported = out
        .stdout
        .lines()
        .find_map(|l| l.strip_prefix("installed="))
        .map(str::trim);
    if reported != Some(plan.version.as_str()) {
        return Err(InstallError::Remote {
            what: "unpacking FramePlayer".into(),
            code: out.code,
            detail: format!("unexpected output: {}", out.stdout.trim()),
        });
    }
    Ok(())
}

/// Adds the shortcut with the chosen method.
fn register_shortcut(
    remote: &mut dyn Remote,
    device: &DeviceInfo,
    spec: &ShortcutSpec,
    info: &ArchiveInfo,
    opts: &InstallOptions,
) -> Result<()> {
    let try_devkit = match opts.shortcut {
        ShortcutMethod::None => {
            println!(
                "Skipping the Steam library. To add it yourself: Steam > Add a Non-Steam Game > {}",
                spec.exe
            );
            return Ok(());
        }
        ShortcutMethod::Devkit => true,
        ShortcutMethod::Auto => device.has_devkit_shortcut,
        ShortcutMethod::Vdf => false,
    };
    if try_devkit {
        match register_with_devkit_tool(remote, device, spec) {
            Ok(()) => {
                println!("Added to the Steam library with Valve's devkit tools.");
                return Ok(());
            }
            Err(e) if opts.shortcut == ShortcutMethod::Auto => {
                println!(
                    "Valve's devkit shortcut tool did not work ({e}); editing Steam's shortcut list instead."
                );
            }
            Err(e) => return Err(e),
        }
    }
    register_with_vdf(remote, device, spec, info)?;
    if opts.restart_steam {
        restart_steam(remote)?;
    } else {
        println!(
            "Steam must restart to show FramePlayer in the library: restart the headset, or \
             run this again with --restart-steam."
        );
    }
    Ok(())
}

/// Registers via `~/devkit-utils/steam-client-create-shortcut`. The
/// argument format is unverified; see [`devkit_shortcut_command`].
fn register_with_devkit_tool(
    remote: &mut dyn Remote,
    device: &DeviceInfo,
    spec: &ShortcutSpec,
) -> Result<()> {
    if !device.has_devkit_shortcut {
        return Err(InstallError::Requirement(format!(
            "~/{DEVKIT_SHORTCUT_TOOL} is not on the headset"
        )));
    }
    let tool = device.home_path(DEVKIT_SHORTCUT_TOOL);
    run_checked(
        remote,
        "Valve's devkit shortcut tool",
        &devkit_shortcut_command(&tool, DEVKIT_GAME_ID, spec),
    )?;
    Ok(())
}

/// Downloads, parses and returns one account's `shortcuts.vdf` (empty if
/// the account has none yet).
fn read_shortcuts(
    remote: &mut dyn Remote,
    device: &DeviceInfo,
    user: &str,
    tmp: &Path,
) -> Result<vdf::Map> {
    let path = format!("{}/shortcuts.vdf", device.steam_config_dir(user));
    if !device.steam_users_with_shortcuts.iter().any(|u| u == user) {
        return Ok(vdf::Map::new());
    }
    let local = tmp.join(format!("shortcuts-{user}.vdf"));
    remote.download(&path, &local)?;
    let data = std::fs::read(&local).map_err(|e| InstallError::io("cannot read", &local, e))?;
    vdf::parse(&data).map_err(|source| InstallError::Vdf { path, source })
}

/// Backs up and replaces one account's `shortcuts.vdf` with `doc`.
fn write_shortcuts(
    remote: &mut dyn Remote,
    device: &DeviceInfo,
    user: &str,
    tmp: &Path,
    doc: &vdf::Map,
) -> Result<()> {
    let config = device.steam_config_dir(user);
    let path = format!("{config}/shortcuts.vdf");
    let bytes = vdf::write(doc).map_err(|source| InstallError::Vdf {
        path: path.clone(),
        source,
    })?;
    let local = tmp.join(format!("shortcuts-{user}.new.vdf"));
    std::fs::write(&local, bytes).map_err(|e| InstallError::io("cannot write", &local, e))?;
    let staged = format!("{path}.frameplayer-new");
    let backup = format!("{path}.frameplayer-bak");
    run_checked(
        remote,
        "backing up Steam's shortcut list",
        &format!(
            "set -e; mkdir -p {c}; if [ -f {p} ]; then cp -f {p} {b}; fi",
            c = sh_quote(&config),
            p = sh_quote(&path),
            b = sh_quote(&backup),
        ),
    )?;
    remote.upload(&local, &staged)?;
    run_checked(
        remote,
        "saving Steam's shortcut list",
        &format!("mv -f {} {}", sh_quote(&staged), sh_quote(&path)),
    )?;
    Ok(())
}

fn temp_dir() -> Result<tempfile::TempDir> {
    tempfile::tempdir().map_err(|e| InstallError::io("cannot create", std::env::temp_dir(), e))
}

/// Edits every Steam account's `shortcuts.vdf` and copies the artwork.
fn register_with_vdf(
    remote: &mut dyn Remote,
    device: &DeviceInfo,
    spec: &ShortcutSpec,
    info: &ArchiveInfo,
) -> Result<()> {
    if device.steam_users.is_empty() {
        println!(
            "No Steam account was found on the headset (sign in to Steam on it first). \
             To add FramePlayer by hand: Steam > Add a Non-Steam Game > {}",
            spec.exe
        );
        return Ok(());
    }
    let tmp = temp_dir()?;
    let app_id = spec.app_id();
    for user in &device.steam_users {
        let mut doc = read_shortcuts(remote, device, user, tmp.path())?;
        let what = upsert_shortcut(&mut doc, spec);
        write_shortcuts(remote, device, user, tmp.path(), &doc)?;
        let art = copy_grid_art(remote, device, user, &spec.start_dir, app_id, info)?;
        let verb = match what {
            Upsert::Added => "Added",
            Upsert::Updated => "Updated",
        };
        println!(
            "{verb} \"{}\" in the Steam library of account {user}{}.",
            spec.app_name,
            if art > 0 {
                format!(" with {art} artwork image(s)")
            } else {
                String::new()
            }
        );
    }
    Ok(())
}

/// The shell script copying the release's library artwork into an
/// account's `config/grid/`. Returns `None` when the release has none.
pub fn grid_art_script(
    grid_dir: &str,
    install_dir: &str,
    app_id: u32,
    info: &ArchiveInfo,
) -> Option<(String, usize)> {
    let mut lines = vec![format!("set -e; mkdir -p {}", sh_quote(grid_dir))];
    for (src, template) in GRID_ART {
        let rel = format!("{STEAM_ASSETS_DIR}/{src}");
        if info.steam_assets.contains(&rel) {
            lines.push(format!(
                "cp -f {} {}",
                sh_quote(&format!("{install_dir}/{rel}")),
                sh_quote(&format!("{grid_dir}/{}", grid_file_name(template, app_id)))
            ));
        }
    }
    let n = lines.len() - 1;
    (n > 0).then(|| (lines.join("\n"), n))
}

fn copy_grid_art(
    remote: &mut dyn Remote,
    device: &DeviceInfo,
    user: &str,
    install_dir: &str,
    app_id: u32,
    info: &ArchiveInfo,
) -> Result<usize> {
    let grid = format!("{}/grid", device.steam_config_dir(user));
    match grid_art_script(&grid, install_dir, app_id, info) {
        Some((script, n)) => {
            run_checked(remote, "copying library artwork", &script)?;
            Ok(n)
        }
        None => Ok(0),
    }
}

/// Script asking a running Steam to shut down. In Gaming Mode the session
/// starts it again, which makes it re-read `shortcuts.vdf`.
pub const RESTART_STEAM_SCRIPT: &str = r#"
if ! pgrep -x steam >/dev/null 2>&1; then echo "steam=not-running"; exit 0; fi
command -v steam >/dev/null 2>&1 || { echo "the steam command is not on PATH" >&2; exit 1; }
nohup steam -shutdown </dev/null >/dev/null 2>&1 &
echo "steam=shutdown"
"#;

fn restart_steam(remote: &mut dyn Remote) -> Result<()> {
    let out = remote.run(RESTART_STEAM_SCRIPT)?;
    if out.stdout.contains("steam=not-running") {
        println!("Steam is not running; it will load FramePlayer the next time it starts.");
    } else if out.success() {
        println!(
            "Asked Steam to restart. In Gaming Mode it comes back by itself; if it does not, \
             restart the headset."
        );
    } else {
        println!(
            "Could not restart Steam ({}). Restart the headset to see FramePlayer in the library.",
            out.stderr.trim()
        );
    }
    Ok(())
}

/// Removes FramePlayer from the headset: Steam shortcuts and artwork, the
/// install folder with its `.old`/`.new` siblings, and (with `purge`) the
/// user's settings, library database and caches.
pub fn uninstall(
    remote: &mut dyn Remote,
    device: &DeviceInfo,
    opts: &UninstallOptions,
) -> Result<()> {
    let dir = resolve_install_dir(&device.home, &opts.dir)?;
    let spec = ShortcutSpec {
        app_name: opts.app_name.clone(),
        exe: format!("{dir}/{LAUNCHER_NAME}"),
        start_dir: dir.clone(),
        icon: String::new(),
        launch_options: String::new(),
    };
    let bin_spec = ShortcutSpec {
        exe: format!("{dir}/{BINARY_NAME}"),
        ..spec.clone()
    };
    let tmp = temp_dir()?;
    let mut removed_any = false;
    for user in &device.steam_users_with_shortcuts {
        let mut doc = read_shortcuts(remote, device, user, tmp.path())?;
        let mut ids = Vec::new();
        for s in [&spec, &bin_spec] {
            if let Some(id) = shortcut_app_id_in(&doc, s) {
                ids.push(id);
            }
            ids.push(s.app_id());
        }
        let removed = remove_shortcut(&mut doc, &spec) | remove_shortcut(&mut doc, &bin_spec);
        if removed {
            write_shortcuts(remote, device, user, tmp.path(), &doc)?;
            println!("Removed FramePlayer from the Steam library of account {user}.");
            removed_any = true;
        }
        ids.sort_unstable();
        ids.dedup();
        let grid = format!("{}/grid", device.steam_config_dir(user));
        let files: Vec<String> = ids
            .iter()
            .flat_map(|id| {
                GRID_ART
                    .iter()
                    .map(|(_, t)| sh_quote(&format!("{grid}/{}", grid_file_name(t, *id))))
                    .collect::<Vec<_>>()
            })
            .collect();
        run_checked(
            remote,
            "removing library artwork",
            &format!("rm -f {}", files.join(" ")),
        )?;
    }
    if device.has_devkit_shortcut {
        println!(
            "If FramePlayer was added with Valve's devkit tools, remove it from Library > \
             Non-Steam (Devkit Game) on the headset."
        );
    }

    let mut targets: Vec<String> = ["", ".old", ".new", ".unpack", ".rollback"]
        .iter()
        .map(|s| sh_quote(&format!("{dir}{s}")))
        .collect();
    targets.push(sh_quote(&device.home_path(REMOTE_STAGING)));
    let mut script = format!("rm -rf {}", targets.join(" "));
    if opts.purge {
        // Same locations as fp_core::dirs, resolved with the headset's
        // own XDG variables.
        script.push_str(
            "\nrm -rf \"${XDG_CONFIG_HOME:-$HOME/.config}/frameplayer\" \
             \"${XDG_DATA_HOME:-$HOME/.local/share}/frameplayer\" \
             \"${XDG_CACHE_HOME:-$HOME/.cache}/frameplayer\"",
        );
    }
    run_checked(remote, "removing FramePlayer's files", &script)?;
    println!("Removed {dir}.");
    if opts.purge {
        println!("Removed FramePlayer's settings, library database and caches.");
    } else {
        println!(
            "Your FramePlayer settings and library were kept (use --purge to delete them too)."
        );
    }
    if removed_any {
        if opts.restart_steam {
            restart_steam(remote)?;
        } else {
            println!("Restart Steam (or the headset) for the library to update.");
        }
    }
    Ok(())
}

/// The stored app id of FramePlayer's entry, if present.
fn shortcut_app_id_in(doc: &vdf::Map, spec: &ShortcutSpec) -> Option<u32> {
    let key = find_shortcut(doc, spec)?;
    match doc.get_map("shortcuts")?.get_map(&key)?.get("appid")? {
        Value::Int(i) => Some(u32::from_le_bytes(i.to_le_bytes())),
        _ => None,
    }
}

/// What [`status`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    /// Install folder.
    pub dir: String,
    /// Installed version.
    pub installed: Option<Version>,
    /// Version kept for rollback.
    pub previous: Option<Version>,
    /// Steam accounts whose library has the shortcut.
    pub shortcut_users: Vec<String>,
}

/// Reports what is installed, without changing anything.
pub fn status(remote: &mut dyn Remote, device: &DeviceInfo, dir: &str) -> Result<Status> {
    let dir = resolve_install_dir(&device.home, dir)?;
    let installed = remote_version(remote, &dir)?;
    let previous = remote_version(remote, &format!("{dir}.old"))?;
    let tmp = temp_dir()?;
    let mut shortcut_users = Vec::new();
    for user in &device.steam_users_with_shortcuts {
        let doc = read_shortcuts(remote, device, user, tmp.path())?;
        let found = [LAUNCHER_NAME, BINARY_NAME].iter().any(|program| {
            let spec = ShortcutSpec {
                app_name: String::new(),
                exe: format!("{dir}/{program}"),
                start_dir: dir.clone(),
                icon: String::new(),
                launch_options: String::new(),
            };
            find_shortcut(&doc, &spec).is_some()
        });
        if found {
            shortcut_users.push(user.clone());
        }
    }
    match &installed {
        Some(v) => println!("Installed: FramePlayer {v} in {dir}"),
        None => println!("FramePlayer is not installed in {dir}."),
    }
    if let Some(v) = &previous {
        println!("Previous version kept for rollback: {v}");
    }
    if shortcut_users.is_empty() {
        println!("Steam library: no FramePlayer shortcut found.");
    } else {
        println!(
            "Steam library: shortcut present for account(s) {}.",
            shortcut_users.join(", ")
        );
    }
    if device.has_devkit_shortcut {
        println!("Valve's devkit tools are present (devkit shortcuts are not listed here).");
    }
    println!(
        "Steam is {}.",
        if device.steam_running {
            "running"
        } else {
            "not running"
        }
    );
    Ok(Status {
        dir,
        installed,
        previous,
        shortcut_users,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::tests::{FakeRemote, sh_unquote};
    use crate::shortcut::app_id_as_vdf_int;
    use std::io::Write as _;
    use zip::write::SimpleFileOptions;

    fn release_zip(dir: &Path, version: &str, art: bool) -> PathBuf {
        let path = dir.join(format!("frameplayer-{version}-aarch64.zip"));
        let mut w = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
        let exec = SimpleFileOptions::default().unix_permissions(0o755);
        let plain = SimpleFileOptions::default().unix_permissions(0o644);
        w.start_file("frameplayer/frameplayer", exec).unwrap();
        w.write_all(b"\x7fELF").unwrap();
        w.start_file("frameplayer/frameplayer.sh", exec).unwrap();
        w.write_all(b"#!/bin/sh\n").unwrap();
        w.start_file("frameplayer/VERSION", plain).unwrap();
        w.write_all(format!("{version}\n").as_bytes()).unwrap();
        if art {
            for f in ["portrait.png", "hero.png", "logo.png", "icon.png"] {
                w.start_file(format!("frameplayer/assets/steam/{f}"), plain)
                    .unwrap();
                w.write_all(b"png").unwrap();
            }
        }
        w.finish().unwrap();
        path
    }

    const HOME: &str = "/home/steam";
    const VDF: &str = "/home/steam/.local/share/Steam/userdata/42/config/shortcuts.vdf";

    fn device() -> DeviceInfo {
        DeviceInfo::parse(
            "arch=aarch64\nhome=/home/steam\nos_id=steamos\nunzip=1\npython3=1\n\
             free_kb=10000000\nsteam_user=42\nsteam_vdf=42\nsteam_running=1\n",
        )
    }

    fn opts() -> InstallOptions {
        InstallOptions {
            dir: DEFAULT_INSTALL_DIR.into(),
            app_name: "FramePlayer".into(),
            shortcut: ShortcutMethod::Auto,
            restart_steam: false,
            reinstall: false,
        }
    }

    /// A shortcuts.vdf holding one unrelated game.
    fn existing_vdf() -> Vec<u8> {
        let mut entry = vdf::Map::new();
        entry.set("appid", Value::Int(-1));
        entry.set("AppName", Value::Str("Other".into()));
        entry.set("Exe", Value::Str("\"/usr/bin/other\"".into()));
        let mut list = vdf::Map::new();
        list.set("0", Value::Map(entry));
        let mut doc = vdf::Map::new();
        doc.set("shortcuts", Value::Map(list));
        vdf::write(&doc).unwrap()
    }

    fn fake() -> FakeRemote {
        let mut f = FakeRemote::default();
        f.on("unzip -q", "installed=1.2.3\n");
        f.files.insert(VDF.into(), existing_vdf());
        f
    }

    #[test]
    fn install_script_shape() {
        let p = RemotePlan {
            dir: "/home/steam/frameplayer".into(),
            zip: "/home/steam/.cache/frameplayer-install/frameplayer-1.2.3.zip".into(),
            sha256: "ab".repeat(32),
            version: "1.2.3".into(),
            root: Some("frameplayer".into()),
        };
        let s = install_script(&p);
        assert!(s.starts_with("set -eu\n"));
        assert!(s.contains("dir='/home/steam/frameplayer'"));
        assert!(s.contains(&format!("want_sha='{}'", "ab".repeat(32))));
        assert!(s.contains("unzip -q -o \"$zip\" -d \"$unpack\""));
        assert!(s.contains("zipfile.ZipFile(sys.argv[1]).extractall(sys.argv[2])"));
        assert!(s.contains("mv \"$unpack\"/'frameplayer' \"$new\""));
        assert!(s.contains("chmod 755 \"$new/frameplayer\""));
        assert!(s.contains("chmod 755 \"$new/frameplayer.sh\""));
        assert!(s.contains("rm -rf \"$old\"; mv \"$dir\" \"$old\""));
        assert!(s.contains("echo \"installed=$ver\""));
        // The python snippets must not contain a single quote, which would
        // end the sh quoting early.
        for chunk in s.split("python3 -c '").skip(1) {
            let code = chunk.split('\'').next().unwrap();
            assert!(code.contains("sys.argv[1]"), "{code}");
        }
        let flat = install_script(&RemotePlan { root: None, ..p });
        assert!(flat.contains("mv \"$unpack\" \"$new\""));
    }

    #[test]
    fn full_install_with_vdf_and_art() {
        let tmp = tempfile::tempdir().unwrap();
        let zip = release_zip(tmp.path(), "1.2.3", true);
        let mut f = fake();
        let v = install_zip(&mut f, &device(), &zip, &opts()).unwrap();
        assert_eq!(v, Version::new(1, 2, 3));

        // Zip uploaded to staging, then the install script ran with the
        // right digest.
        let staged = "/home/steam/.cache/frameplayer-install/frameplayer-1.2.3.zip";
        assert_eq!(f.uploads[0].1, staged);
        let (_, sha) = hash_file(&zip).unwrap();
        assert!(f.ran(&format!("want_sha='{sha}'")));
        assert!(f.ran("mkdir -p '/home/steam/.cache/frameplayer-install'"));

        // shortcuts.vdf backed up, replaced atomically, and holds both
        // entries.
        assert!(f.ran(&format!("cp -f '{VDF}' '{VDF}.frameplayer-bak'")));
        assert!(f.ran(&format!("mv -f '{VDF}.frameplayer-new' '{VDF}'")));
        let doc = vdf::parse(&f.files[&format!("{VDF}.frameplayer-new")]).unwrap();
        let list = doc.get_map("shortcuts").unwrap();
        assert_eq!(list.0.len(), 2);
        let e = list.get_map("1").unwrap();
        assert_eq!(
            e.get_str("Exe"),
            Some("\"/home/steam/frameplayer/frameplayer.sh\"")
        );
        assert_eq!(
            e.get_str("icon"),
            Some("/home/steam/frameplayer/assets/steam/icon.png")
        );
        let spec = shortcut_spec(
            "/home/steam/frameplayer",
            "FramePlayer",
            &inspect_zip(&zip).unwrap(),
        );
        assert_eq!(e.get_int("appid"), Some(app_id_as_vdf_int(spec.app_id())));

        // Artwork copied under the app id; no capsule.png in this zip.
        let id = spec.app_id();
        let grid = "/home/steam/.local/share/Steam/userdata/42/config/grid";
        assert!(f.ran(&format!(
            "cp -f '/home/steam/frameplayer/assets/steam/portrait.png' '{grid}/{id}p.png'"
        )));
        assert!(f.ran(&format!("'{grid}/{id}_hero.png'")));
        assert!(f.ran(&format!("'{grid}/{id}_logo.png'")));
        assert!(!f.ran(&format!("'{grid}/{id}.png'")));
        // Steam was not touched without --restart-steam.
        assert!(!f.ran("steam -shutdown"));
    }

    #[test]
    fn same_version_skips_copy_but_fixes_shortcut() {
        let tmp = tempfile::tempdir().unwrap();
        let zip = release_zip(tmp.path(), "1.2.3", false);
        let mut f = fake();
        f.rules.insert(
            0,
            (
                "frameplayer/VERSION".into(),
                crate::remote::CmdOutput {
                    code: Some(0),
                    stdout: "1.2.3\n".into(),
                    stderr: String::new(),
                },
            ),
        );
        install_zip(&mut f, &device(), &zip, &opts()).unwrap();
        assert!(!f.ran("unzip -q"));
        assert!(f.uploads.iter().all(|(_, r)| !r.ends_with(".zip")));
        assert!(f.files.contains_key(&format!("{VDF}.frameplayer-new")));

        // --reinstall copies again.
        let mut f2 = fake();
        f2.rules = f.rules.clone();
        let o = InstallOptions {
            reinstall: true,
            ..opts()
        };
        install_zip(&mut f2, &device(), &zip, &o).unwrap();
        assert!(f2.ran("unzip -q"));
    }

    #[test]
    fn devkit_tool_used_first_and_falls_back() {
        let tmp = tempfile::tempdir().unwrap();
        let zip = release_zip(tmp.path(), "1.2.3", false);
        let mut d = device();
        d.has_devkit_shortcut = true;

        // Tool works: shortcuts.vdf untouched.
        let mut f = fake();
        install_zip(&mut f, &d, &zip, &opts()).unwrap();
        let call = f
            .scripts
            .iter()
            .find(|s| s.contains("steam-client-create-shortcut"))
            .unwrap();
        assert!(
            call.starts_with("'/home/steam/devkit-utils/steam-client-create-shortcut' --parms '")
        );
        let json = sh_unquote(call.split_once(" --parms ").unwrap().1);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["argv"][0], "/home/steam/frameplayer/frameplayer.sh");
        assert!(!f.files.contains_key(&format!("{VDF}.frameplayer-new")));

        // Tool fails: falls back to editing shortcuts.vdf.
        let mut f = fake();
        f.fail("steam-client-create-shortcut", 2, "usage: ...");
        install_zip(&mut f, &d, &zip, &opts()).unwrap();
        assert!(f.files.contains_key(&format!("{VDF}.frameplayer-new")));

        // Forced devkit method: failure is an error.
        let mut f = fake();
        f.fail("steam-client-create-shortcut", 2, "usage: ...");
        let o = InstallOptions {
            shortcut: ShortcutMethod::Devkit,
            ..opts()
        };
        assert!(matches!(
            install_zip(&mut f, &d, &zip, &o),
            Err(InstallError::Remote { .. })
        ));
    }

    #[test]
    fn restart_steam_after_vdf_edit() {
        let tmp = tempfile::tempdir().unwrap();
        let zip = release_zip(tmp.path(), "1.2.3", false);
        let mut f = fake();
        f.on("steam -shutdown", "steam=shutdown\n");
        let o = InstallOptions {
            restart_steam: true,
            ..opts()
        };
        install_zip(&mut f, &device(), &zip, &o).unwrap();
        let mv = f
            .scripts
            .iter()
            .position(|s| s.contains(".frameplayer-new' '"))
            .unwrap();
        let stop = f
            .scripts
            .iter()
            .position(|s| s.contains("steam -shutdown"))
            .unwrap();
        assert!(stop > mv, "Steam is restarted after the list is saved");
    }

    #[test]
    fn requirements_checked_before_upload() {
        let tmp = tempfile::tempdir().unwrap();
        let zip = release_zip(tmp.path(), "1.2.3", false);
        let mut d = device();
        d.free_bytes = Some(1000);
        let mut f = fake();
        assert!(matches!(
            install_zip(&mut f, &d, &zip, &opts()),
            Err(InstallError::Requirement(_))
        ));
        let mut d = device();
        d.has_unzip = false;
        d.has_python3 = false;
        assert!(matches!(
            install_zip(&mut f, &d, &zip, &opts()),
            Err(InstallError::Requirement(_))
        ));
        assert!(f.uploads.is_empty());
    }

    #[test]
    fn unsafe_zip_never_uploaded() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("evil.zip");
        let mut w = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
        let o = SimpleFileOptions::default().unix_permissions(0o755);
        w.start_file("frameplayer/frameplayer", o).unwrap();
        w.start_file("frameplayer/VERSION", o).unwrap();
        w.write_all(b"1.0.0").unwrap();
        w.start_file("frameplayer/../../.bashrc", o).unwrap();
        w.finish().unwrap();
        let mut f = fake();
        assert!(matches!(
            install_zip(&mut f, &device(), &path, &opts()),
            Err(InstallError::Updater(fp_updater::Error::UnsafeArchivePath(
                _
            )))
        ));
        assert!(f.uploads.is_empty());
        assert!(f.scripts.is_empty());
    }

    #[test]
    fn script_failure_is_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let zip = release_zip(tmp.path(), "1.2.3", false);
        let mut f = FakeRemote::default();
        f.fail("unzip -q", 1, "the uploaded zip is damaged (SHA-256 00)");
        let err = install_zip(&mut f, &device(), &zip, &opts()).unwrap_err();
        assert!(err.to_string().contains("damaged"), "{err}");
        // Shortcut not touched after a failed install.
        assert!(!f.ran("frameplayer-bak"));
    }

    #[test]
    fn uninstall_removes_everything() {
        let tmp = tempfile::tempdir().unwrap();
        let zip = release_zip(tmp.path(), "1.2.3", true);
        let mut f = fake();
        install_zip(&mut f, &device(), &zip, &opts()).unwrap();
        // The headset now has the new list.
        let new = f.files[&format!("{VDF}.frameplayer-new")].clone();
        f.files.insert(VDF.into(), new);
        f.scripts.clear();

        let o = UninstallOptions {
            dir: "~/frameplayer".into(),
            app_name: "FramePlayer".into(),
            purge: false,
            restart_steam: false,
        };
        uninstall(&mut f, &device(), &o).unwrap();
        let doc = vdf::parse(&f.files[&format!("{VDF}.frameplayer-new")]).unwrap();
        let list = doc.get_map("shortcuts").unwrap();
        assert_eq!(list.0.len(), 1);
        assert_eq!(list.get_map("0").unwrap().get_str("AppName"), Some("Other"));
        let rm = f
            .scripts
            .iter()
            .find(|s| s.starts_with("rm -rf '/home/steam/frameplayer'"))
            .unwrap();
        assert!(rm.contains("'/home/steam/frameplayer.old'"));
        assert!(!rm.contains("XDG_CONFIG_HOME"));
        let spec = shortcut_spec(
            "/home/steam/frameplayer",
            "FramePlayer",
            &inspect_zip(&zip).unwrap(),
        );
        assert!(f.ran(&format!("{}p.png", spec.app_id())));

        f.scripts.clear();
        let o = UninstallOptions { purge: true, ..o };
        uninstall(&mut f, &device(), &o).unwrap();
        assert!(f.ran("${XDG_CONFIG_HOME:-$HOME/.config}/frameplayer"));
    }

    #[test]
    fn uninstall_refuses_home() {
        let mut f = fake();
        let o = UninstallOptions {
            dir: HOME.into(),
            app_name: "FramePlayer".into(),
            purge: true,
            restart_steam: false,
        };
        assert!(matches!(
            uninstall(&mut f, &device(), &o),
            Err(InstallError::BadPath(_))
        ));
        assert!(f.scripts.is_empty());
    }

    #[test]
    fn status_reports() {
        let tmp = tempfile::tempdir().unwrap();
        let zip = release_zip(tmp.path(), "1.2.3", false);
        let mut f = fake();
        install_zip(&mut f, &device(), &zip, &opts()).unwrap();
        let new = f.files[&format!("{VDF}.frameplayer-new")].clone();
        f.files.insert(VDF.into(), new);
        f.rules.insert(
            0,
            (
                "frameplayer.old/VERSION".into(),
                crate::remote::CmdOutput {
                    code: Some(0),
                    stdout: "1.0.0".into(),
                    stderr: String::new(),
                },
            ),
        );
        f.rules.insert(
            1,
            (
                "frameplayer/VERSION".into(),
                crate::remote::CmdOutput {
                    code: Some(0),
                    stdout: "1.2.3".into(),
                    stderr: String::new(),
                },
            ),
        );
        let s = status(&mut f, &device(), DEFAULT_INSTALL_DIR).unwrap();
        assert_eq!(s.installed, Some(Version::new(1, 2, 3)));
        assert_eq!(s.previous, Some(Version::new(1, 0, 0)));
        assert_eq!(s.shortcut_users, ["42"]);
    }

    /// Runs scripts with the local `sh`, with `HOME` set to a temporary
    /// folder: the real install scripts, end to end, minus ssh.
    #[cfg(unix)]
    struct LocalShell {
        home: PathBuf,
        path: Option<String>,
    }

    #[cfg(unix)]
    impl Remote for LocalShell {
        fn run(&mut self, script: &str) -> Result<crate::remote::CmdOutput> {
            use std::process::{Command, Stdio};
            let mut cmd = Command::new("/bin/sh");
            cmd.arg("-s")
                .env("HOME", &self.home)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            if let Some(p) = &self.path {
                cmd.env("PATH", p);
            }
            let mut child = cmd.spawn().unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(script.as_bytes())
                .unwrap();
            let out = child.wait_with_output().unwrap();
            Ok(crate::remote::CmdOutput {
                code: out.status.code(),
                stdout: String::from_utf8_lossy(&out.stdout).into(),
                stderr: String::from_utf8_lossy(&out.stderr).into(),
            })
        }
        fn upload(&mut self, local: &Path, remote: &str) -> Result<()> {
            std::fs::copy(local, remote).unwrap();
            Ok(())
        }
        fn download(&mut self, remote: &str, local: &Path) -> Result<()> {
            std::fs::copy(remote, local).unwrap();
            Ok(())
        }
        fn host(&self) -> &str {
            "local-sh"
        }
    }

    /// A PATH holding only the tools the scripts may use, minus `skip`.
    #[cfg(unix)]
    fn restricted_path(dir: &Path, skip: &[&str]) -> Option<String> {
        let tools = [
            "sh",
            "uname",
            "awk",
            "df",
            "basename",
            "pgrep",
            "sha256sum",
            "cut",
            "rm",
            "mkdir",
            "dirname",
            "mv",
            "head",
            "tr",
            "chmod",
            "cp",
            "cat",
            "unzip",
            "python3",
        ];
        std::fs::create_dir_all(dir).unwrap();
        for t in tools.iter().filter(|t| !skip.contains(t)) {
            let found = ["/usr/bin", "/bin"]
                .iter()
                .map(|d| Path::new(d).join(t))
                .find(|p| p.exists());
            match found {
                Some(p) => std::os::unix::fs::symlink(p, dir.join(t)).unwrap(),
                None if *t == "unzip" || *t == "python3" => {}
                None => return None,
            }
        }
        Some(dir.display().to_string())
    }

    #[cfg(unix)]
    fn local_install(skip: &[&str]) {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let userdata = home.join(".local/share/Steam/userdata/42/config");
        std::fs::create_dir_all(&userdata).unwrap();
        std::fs::write(userdata.join("shortcuts.vdf"), existing_vdf()).unwrap();
        let Some(path) = restricted_path(&tmp.path().join("bin"), skip) else {
            eprintln!("skipping: required tools missing");
            return;
        };
        let mut sh = LocalShell {
            home: home.clone(),
            path: Some(path),
        };
        let device = probe(&mut sh).unwrap();
        if (!skip.contains(&"unzip") && !device.has_unzip) || !device.has_python3 {
            eprintln!("skipping: unzip or python3 missing on this machine");
            return;
        }
        assert_eq!(device.has_unzip, !skip.contains(&"unzip"));
        assert_eq!(device.steam_users_with_shortcuts, ["42"]);
        device.check(true).unwrap();

        let z1 = release_zip(tmp.path(), "1.0.0", true);
        let z2 = release_zip(tmp.path(), "1.1.0", true);
        install_zip(&mut sh, &device, &z1, &opts()).unwrap();
        let dir = home.join("frameplayer");
        assert_eq!(
            std::fs::read_to_string(dir.join("VERSION")).unwrap(),
            "1.0.0\n"
        );
        let mode = std::fs::metadata(dir.join("frameplayer"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);
        assert!(dir.join("assets/steam/hero.png").is_file());
        assert!(!home.join("frameplayer.unpack").exists());
        assert!(!home.join("frameplayer.new").exists());

        // Second version: previous kept as .old, matching fp-updater's
        // layout so the in-app rollback works.
        let device = probe(&mut sh).unwrap();
        install_zip(&mut sh, &device, &z2, &opts()).unwrap();
        assert_eq!(
            fp_updater::installed_version(&dir).unwrap(),
            Some(Version::new(1, 1, 0))
        );
        let old = home.join("frameplayer.old");
        assert_eq!(
            fp_updater::installed_version(&old).unwrap(),
            Some(Version::new(1, 0, 0))
        );
        assert_eq!(
            fp_updater::rollback(&dir).unwrap(),
            Version::new(1, 0, 0),
            "fp-updater can roll back an install made by the PC installer"
        );
        fp_updater::rollback(&dir).unwrap();

        // Steam list: original kept as backup, new list has both games,
        // artwork in place.
        let vdf_path = userdata.join("shortcuts.vdf");
        // The backup is the list as it was before the last edit.
        let backup =
            vdf::parse(&std::fs::read(userdata.join("shortcuts.vdf.frameplayer-bak")).unwrap())
                .unwrap();
        assert_eq!(backup.get_map("shortcuts").unwrap().0.len(), 2);
        let doc = vdf::parse(&std::fs::read(&vdf_path).unwrap()).unwrap();
        assert_eq!(doc.get_map("shortcuts").unwrap().0.len(), 2);
        let spec = shortcut_spec(
            &dir.display().to_string(),
            "FramePlayer",
            &inspect_zip(&z2).unwrap(),
        );
        let id = spec.app_id();
        assert!(userdata.join(format!("grid/{id}p.png")).is_file());
        assert!(userdata.join(format!("grid/{id}_hero.png")).is_file());

        // A damaged upload is refused and the install is left alone.
        let d = probe(&mut sh).unwrap();
        let err = upload_and_unpack(
            &mut sh,
            &d,
            &z1,
            &dir.display().to_string(),
            &inspect_zip(&z1).unwrap(),
            1,
            "00".repeat(32),
        )
        .unwrap_err();
        assert!(err.to_string().contains("damaged"), "{err}");
        assert_eq!(
            fp_updater::installed_version(&dir).unwrap(),
            Some(Version::new(1, 1, 0))
        );

        // Status, then uninstall with purge.
        let d = probe(&mut sh).unwrap();
        let st = status(&mut sh, &d, "frameplayer").unwrap();
        assert_eq!(st.installed, Some(Version::new(1, 1, 0)));
        assert_eq!(st.shortcut_users, ["42"]);
        std::fs::create_dir_all(home.join(".config/frameplayer")).unwrap();
        let o = UninstallOptions {
            dir: "frameplayer".into(),
            app_name: "FramePlayer".into(),
            purge: true,
            restart_steam: false,
        };
        let d = probe(&mut sh).unwrap();
        uninstall(&mut sh, &d, &o).unwrap();
        assert!(!dir.exists() && !old.exists());
        assert!(!home.join(".config/frameplayer").exists());
        assert!(!userdata.join(format!("grid/{id}p.png")).exists());
        let doc = vdf::parse(&std::fs::read(&vdf_path).unwrap()).unwrap();
        assert_eq!(doc.get_map("shortcuts").unwrap().0.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn scripts_run_for_real_with_unzip() {
        local_install(&[]);
    }

    #[cfg(unix)]
    #[test]
    fn scripts_run_for_real_with_python_fallback() {
        local_install(&["unzip", "sha256sum"]);
    }

    #[test]
    fn grid_script_none_without_art() {
        let tmp = tempfile::tempdir().unwrap();
        let info = inspect_zip(&release_zip(tmp.path(), "1.0.0", false)).unwrap();
        assert!(grid_art_script("/g", "/home/steam/frameplayer", 1, &info).is_none());
    }
}
