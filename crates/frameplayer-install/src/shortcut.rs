//! Non-Steam shortcut for FramePlayer: the `shortcuts.vdf` entry, its app
//! id, library artwork names, and the devkit-utils helper invocation.

use crate::remote::sh_quote;
use crate::vdf::{Map, Value};

/// Name shown in the Steam library.
pub const DEFAULT_APP_NAME: &str = "FramePlayer";
/// Tag added to the shortcut (shows up as a collection filter).
pub const TAG: &str = "VR";

/// Everything needed to describe the shortcut.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShortcutSpec {
    /// Library name.
    pub app_name: String,
    /// Absolute path of the program Steam runs (`.../frameplayer.sh`).
    pub exe: String,
    /// Absolute working directory (the install directory).
    pub start_dir: String,
    /// Absolute path of the icon, or empty.
    pub icon: String,
    /// Extra command-line arguments, usually empty.
    pub launch_options: String,
}

impl ShortcutSpec {
    /// The `Exe` value as Steam stores it: the path in double quotes.
    pub fn quoted_exe(&self) -> String {
        quote(&self.exe)
    }

    /// The shortcut's app id. See [`shortcut_app_id`].
    pub fn app_id(&self) -> u32 {
        shortcut_app_id(&self.quoted_exe(), &self.app_name)
    }
}

fn quote(path: &str) -> String {
    format!("\"{path}\"")
}

/// Steam's app id for a non-Steam shortcut:
/// `crc32(exe + app_name) | 0x80000000`, where `exe` is the `Exe` value
/// exactly as stored in `shortcuts.vdf` (including its quotes).
///
/// This is the formula used by Steam ROM Manager and similar tools for
/// shortcuts written straight into `shortcuts.vdf`; the same unsigned value
/// names the artwork files in `config/grid/`. In the file it is stored as a
/// signed int32 (see [`app_id_as_vdf_int`]).
pub fn shortcut_app_id(exe: &str, app_name: &str) -> u32 {
    let mut h = crc32fast::Hasher::new();
    h.update(exe.as_bytes());
    h.update(app_name.as_bytes());
    h.finalize() | 0x8000_0000
}

/// The app id reinterpreted as the signed int32 `shortcuts.vdf` stores.
pub fn app_id_as_vdf_int(app_id: u32) -> i32 {
    i32::from_le_bytes(app_id.to_le_bytes())
}

/// Grid artwork: (file name inside the release's `assets/steam/`, file name
/// template in Steam's `config/grid/`, where `{}` is the app id).
///
/// Sizes Steam expects: portrait capsule 600x900, hero 3840x1240, logo
/// (transparent PNG) about 1280x720, wide capsule 920x430.
pub const GRID_ART: [(&str, &str); 4] = [
    ("portrait.png", "{}p.png"),
    ("hero.png", "{}_hero.png"),
    ("logo.png", "{}_logo.png"),
    ("capsule.png", "{}.png"),
];

/// Icon file name inside `assets/steam/`, used for the shortcut's `icon`.
pub const ICON_FILE: &str = "icon.png";

/// Grid file name for `template` and `app_id`.
pub fn grid_file_name(template: &str, app_id: u32) -> String {
    template.replace("{}", &app_id.to_string())
}

/// The map at `key` inside `m`, created (or, if `key` holds a non-map
/// value, replaced) as needed.
fn ensure_map<'a>(m: &'a mut Map, key: &str) -> &'a mut Map {
    let i = match m.0.iter().position(|(k, _)| k.eq_ignore_ascii_case(key)) {
        Some(i) => i,
        None => {
            m.0.push((key.to_string(), Value::Map(Map::new())));
            m.0.len() - 1
        }
    };
    let slot = &mut m.0[i].1;
    if !matches!(slot, Value::Map(_)) {
        *slot = Value::Map(Map::new());
    }
    match slot {
        Value::Map(inner) => inner,
        _ => unreachable!("slot was just made a map"),
    }
}

/// True when `entry` is a FramePlayer shortcut: same app id, or the same
/// `Exe` (so a renamed library entry is still recognised).
fn is_ours(entry: &Map, spec: &ShortcutSpec) -> bool {
    let id = app_id_as_vdf_int(spec.app_id());
    entry.get_int("appid") == Some(id) || entry.get_str("Exe") == Some(spec.quoted_exe().as_str())
}

/// Index of FramePlayer's entry in the `shortcuts` map, if present.
pub fn find_shortcut(doc: &Map, spec: &ShortcutSpec) -> Option<String> {
    doc.get_map("shortcuts")?
        .0
        .iter()
        .find(|(_, v)| matches!(v, Value::Map(e) if is_ours(e, spec)))
        .map(|(k, _)| k.clone())
}

/// Whether the shortcut was added or an existing one updated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Upsert {
    /// A new entry was appended.
    Added,
    /// An existing entry was rewritten.
    Updated,
}

/// Adds FramePlayer's shortcut to a parsed `shortcuts.vdf`, or updates the
/// existing one in place. Fields Steam or the user own (play time, hidden
/// flag, extra tags) are kept on update.
pub fn upsert_shortcut(doc: &mut Map, spec: &ShortcutSpec) -> Upsert {
    let list = ensure_map(doc, "shortcuts");
    let found = list
        .0
        .iter()
        .position(|(_, v)| matches!(v, Value::Map(e) if is_ours(e, spec)));
    let (i, result) = match found {
        Some(i) => (i, Upsert::Updated),
        None => {
            let key = next_index(list).to_string();
            list.0.push((key, Value::Map(Map::new())));
            (list.0.len() - 1, Upsert::Added)
        }
    };
    if let Value::Map(entry) = &mut list.0[i].1 {
        fill_entry(entry, spec, result == Upsert::Added);
    }
    result
}

fn fill_entry(e: &mut Map, spec: &ShortcutSpec, new: bool) {
    let id = app_id_as_vdf_int(spec.app_id());
    e.set("appid", Value::Int(id));
    e.set("AppName", Value::Str(spec.app_name.clone()));
    e.set("Exe", Value::Str(spec.quoted_exe()));
    e.set("StartDir", Value::Str(quote(&spec.start_dir)));
    e.set("icon", Value::Str(spec.icon.clone()));
    e.set("LaunchOptions", Value::Str(spec.launch_options.clone()));
    let defaults: [(&str, Value); 11] = [
        ("ShortcutPath", Value::Str(String::new())),
        ("IsHidden", Value::Int(0)),
        ("AllowDesktopConfig", Value::Int(1)),
        ("AllowOverlay", Value::Int(1)),
        // UNVERIFIED: whether the Frame wants OpenVR=1 for a native OpenXR
        // title. 0 is Steam's default for non-Steam shortcuts.
        ("OpenVR", Value::Int(0)),
        ("Devkit", Value::Int(0)),
        ("DevkitGameID", Value::Str(String::new())),
        ("DevkitOverrideAppID", Value::Int(0)),
        ("LastPlayTime", Value::Int(0)),
        ("FlatpakAppID", Value::Str(String::new())),
        ("tags", Value::Map(Map::new())),
    ];
    for (k, v) in defaults {
        if new || e.get(k).is_none() {
            e.set(k, v);
        }
    }
    if let Some(tags) = e.get_map_mut("tags") {
        if !tags.0.iter().any(|(_, v)| *v == Value::Str(TAG.into())) {
            let k = next_index(tags).to_string();
            tags.0.push((k, Value::Str(TAG.into())));
        }
    }
}

/// Smallest non-negative integer key not yet used.
fn next_index(m: &Map) -> usize {
    (0..)
        .find(|i| m.get(&i.to_string()).is_none())
        .unwrap_or(m.0.len())
}

/// Removes FramePlayer's shortcut and renumbers the remaining entries
/// `0..n` as Steam does. Returns whether anything was removed.
pub fn remove_shortcut(doc: &mut Map, spec: &ShortcutSpec) -> bool {
    let Some(list) = doc.get_map_mut("shortcuts") else {
        return false;
    };
    let before = list.0.len();
    list.0
        .retain(|(_, v)| !matches!(v, Value::Map(e) if is_ours(e, spec)));
    if list.0.len() == before {
        return false;
    }
    for (i, (k, _)) in list.0.iter_mut().enumerate() {
        *k = i.to_string();
    }
    true
}

/// Shell command that asks Valve's devkit helper to create the shortcut.
///
/// UNVERIFIED: `~/devkit-utils/steam-client-create-shortcut` is installed
/// on devkit-enabled SteamOS by the SteamOS Devkit Client. We assume it is
/// a Python script taking `--parms '<json>'` with `gameid`, `directory`,
/// `argv` and `settings`, as the Devkit Client calls it. Its argument
/// format has not been confirmed on a Steam Frame, so this is the only
/// place that knows it, and the installer falls back to editing
/// `shortcuts.vdf` when the command fails.
pub fn devkit_shortcut_command(tool: &str, game_id: &str, spec: &ShortcutSpec) -> String {
    let parms = serde_json::json!({
        "gameid": game_id,
        "directory": spec.start_dir,
        "argv": [spec.exe],
        "settings": { "steam_play": "0" },
    });
    format!(
        "{} --parms {}",
        sh_quote(tool),
        sh_quote(&parms.to_string())
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vdf;

    fn spec() -> ShortcutSpec {
        ShortcutSpec {
            app_name: "FramePlayer".into(),
            exe: "/home/steam/frameplayer/frameplayer.sh".into(),
            start_dir: "/home/steam/frameplayer".into(),
            icon: "/home/steam/frameplayer/assets/steam/icon.png".into(),
            launch_options: String::new(),
        }
    }

    fn other_entry(name: &str) -> Value {
        let mut e = Map::new();
        e.set("appid", Value::Int(-5));
        e.set("AppName", Value::Str(name.into()));
        e.set("Exe", Value::Str(format!("\"/usr/bin/{name}\"")));
        Value::Map(e)
    }

    #[test]
    fn app_id_formula() {
        // Reference: crc32 of the concatenation, high bit set.
        let exe = "\"/home/steam/frameplayer/frameplayer.sh\"";
        let expected = crc32fast::hash(format!("{exe}FramePlayer").as_bytes()) | 0x8000_0000;
        assert_eq!(shortcut_app_id(exe, "FramePlayer"), expected);
        assert_eq!(spec().app_id(), expected);
        assert!(spec().app_id() >= 0x8000_0000);
        // Known crc32 vector: crc32("123456789") = 0xCBF43926.
        assert_eq!(shortcut_app_id("1234", "56789"), 0xCBF4_3926);
        assert_eq!(shortcut_app_id("", ""), 0x8000_0000);
        assert_eq!(app_id_as_vdf_int(0xCBF4_3926), 0xCBF4_3926u32 as i32);
        assert!(app_id_as_vdf_int(spec().app_id()) < 0);
    }

    #[test]
    fn add_update_remove_round_trip() {
        let mut doc = Map::new();
        let mut list = Map::new();
        list.0.push(("0".into(), other_entry("a")));
        list.0.push(("1".into(), other_entry("b")));
        doc.set("shortcuts", Value::Map(list));
        let bytes = vdf::write(&doc).unwrap();

        let mut doc = vdf::parse(&bytes).unwrap();
        assert_eq!(upsert_shortcut(&mut doc, &spec()), Upsert::Added);
        let key = find_shortcut(&doc, &spec()).unwrap();
        assert_eq!(key, "2");

        // Survives a write/parse round trip with the expected fields.
        let doc2 = vdf::parse(&vdf::write(&doc).unwrap()).unwrap();
        assert_eq!(doc2, doc);
        let e = doc2
            .get_map("shortcuts")
            .and_then(|s| s.get_map("2"))
            .unwrap();
        assert_eq!(e.get_int("appid"), Some(app_id_as_vdf_int(spec().app_id())));
        assert_eq!(e.get_str("AppName"), Some("FramePlayer"));
        assert_eq!(
            e.get_str("Exe"),
            Some("\"/home/steam/frameplayer/frameplayer.sh\"")
        );
        assert_eq!(e.get_str("StartDir"), Some("\"/home/steam/frameplayer\""));
        assert_eq!(e.get_str("icon"), Some(spec().icon.as_str()));
        assert_eq!(e.get_str("LaunchOptions"), Some(""));
        assert_eq!(e.get_map("tags").and_then(|t| t.get_str("0")), Some("VR"));

        // Update: user-owned fields survive, ours change, no duplicate.
        let mut doc = doc2;
        if let Some(e) = doc
            .get_map_mut("shortcuts")
            .and_then(|s| s.get_map_mut("2"))
        {
            e.set("LastPlayTime", Value::Int(1_700_000_000));
            e.set("IsHidden", Value::Int(1));
        }
        let mut s2 = spec();
        s2.launch_options = "--verbose".into();
        assert_eq!(upsert_shortcut(&mut doc, &s2), Upsert::Updated);
        let list = doc.get_map("shortcuts").unwrap();
        assert_eq!(list.0.len(), 3);
        let e = list.get_map("2").unwrap();
        assert_eq!(e.get_int("LastPlayTime"), Some(1_700_000_000));
        assert_eq!(e.get_int("IsHidden"), Some(1));
        assert_eq!(e.get_str("LaunchOptions"), Some("--verbose"));
        assert_eq!(e.get_map("tags").unwrap().0.len(), 1);

        // Renamed in the library (different app id) is still found by Exe.
        let mut renamed = spec();
        renamed.app_name = "FramePlayer Beta".into();
        assert_eq!(find_shortcut(&doc, &renamed).as_deref(), Some("2"));

        // Remove: renumbered, others untouched.
        assert!(remove_shortcut(&mut doc, &spec()));
        assert!(!remove_shortcut(&mut doc, &spec()));
        let list = doc.get_map("shortcuts").unwrap();
        let keys: Vec<_> = list.0.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["0", "1"]);
        assert_eq!(list.get_map("1").unwrap().get_str("AppName"), Some("b"));
    }

    #[test]
    fn remove_renumbers_from_middle() {
        let mut doc = Map::new();
        upsert_shortcut(&mut doc, &spec());
        let list = doc.get_map_mut("shortcuts").unwrap();
        list.0.insert(0, ("x".into(), other_entry("a")));
        list.0.push(("y".into(), other_entry("c")));
        assert!(remove_shortcut(&mut doc, &spec()));
        let list = doc.get_map("shortcuts").unwrap();
        let names: Vec<_> = list
            .0
            .iter()
            .map(|(k, v)| match v {
                Value::Map(m) => (k.as_str(), m.get_str("AppName").unwrap_or("")),
                _ => (k.as_str(), ""),
            })
            .collect();
        assert_eq!(names, [("0", "a"), ("1", "c")]);
    }

    #[test]
    fn empty_file_gets_shortcuts_map() {
        let mut doc = vdf::parse(&[]).unwrap();
        assert_eq!(upsert_shortcut(&mut doc, &spec()), Upsert::Added);
        let bytes = vdf::write(&doc).unwrap();
        assert!(bytes.starts_with(b"\x00shortcuts\x00\x000\x00\x02appid\x00"));
        assert!(bytes.ends_with(&[0x08, 0x08, 0x08]));
    }

    #[test]
    fn grid_names() {
        let names: Vec<_> = GRID_ART
            .iter()
            .map(|(_, t)| grid_file_name(t, 2_999_999_999))
            .collect();
        assert_eq!(
            names,
            [
                "2999999999p.png",
                "2999999999_hero.png",
                "2999999999_logo.png",
                "2999999999.png"
            ]
        );
    }

    #[test]
    fn devkit_command_is_quoted_json() {
        let mut s = spec();
        s.exe = "/home/steam/it's/frameplayer.sh".into();
        let cmd = devkit_shortcut_command(
            "/home/steam/devkit-utils/steam-client-create-shortcut",
            "frameplayer",
            &s,
        );
        assert!(
            cmd.starts_with("'/home/steam/devkit-utils/steam-client-create-shortcut' --parms '{")
        );
        // Recover the JSON the way a POSIX shell would and check it.
        let quoted = cmd.split_once(" --parms ").unwrap().1;
        let json = crate::remote::tests::sh_unquote(quoted);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["gameid"], "frameplayer");
        assert_eq!(v["argv"][0], "/home/steam/it's/frameplayer.sh");
        assert_eq!(v["directory"], "/home/steam/frameplayer");
    }
}
