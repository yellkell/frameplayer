//! System facts: OS build, kernel, glibc/libstdc++ baseline, which shared
//! libraries the dynamic linker finds, CPU/memory, user groups, the
//! XR-relevant environment and active OpenXR runtime manifests.
//!
//! Answers P7 (glibc/libstdc++), P9 (system Vulkan loader), P18
//! (`VERSION_ID` numbering), I3 (login name) and the loader half of I11.

use crate::parse;
use crate::redact::{tilde, RedactContext};
use crate::report::{CheckOutput, Status};
use crate::runner::CheckContext;
use crate::util::{self, dlopen_probe, read_trim, Out};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Shared libraries FramePlayer (or its dependencies) loads at run time.
pub const LIBS: &[&str] = &[
    "libstdc++.so.6",
    "libvulkan.so.1",
    "libopenxr_loader.so.1",
    "libopenxr_loader.so",
    "libpipewire-0.3.so.0",
    "libasound.so.2",
    "libdrm.so.2",
    "libgbm.so.1",
    "libEGL.so.1",
];

const OS_KEYS: &[&str] = &[
    "NAME",
    "ID",
    "ID_LIKE",
    "VERSION_ID",
    "VERSION_CODENAME",
    "BUILD_ID",
    "VARIANT_ID",
    "PRETTY_NAME",
    "IMAGE_ID",
    "IMAGE_VERSION",
];

pub fn run(_ctx: &CheckContext) -> CheckOutput {
    let mut o = Out::new();
    let rc = RedactContext::from_env();
    let home = rc.home.clone();

    // OS release.
    let os = read_trim("/etc/os-release")
        .or_else(|| read_trim("/usr/lib/os-release"))
        .map(|s| parse::parse_os_release(&s))
        .unwrap_or_default();
    let os_sel: BTreeMap<&str, &String> = OS_KEYS
        .iter()
        .filter_map(|k| os.get(*k).map(|v| (*k, v)))
        .collect();
    o.set("os_release", &os_sel);
    if os.is_empty() {
        o.finding(
            "os_release",
            Status::Unknown,
            &["P18"],
            "no /etc/os-release",
        );
    } else {
        o.finding(
            "os_release",
            Status::Pass,
            &["P18"],
            format!(
                "{} VERSION_ID={} BUILD_ID={} VARIANT_ID={}",
                os.get("NAME").map_or("?", String::as_str),
                os.get("VERSION_ID").map_or("(none)", String::as_str),
                os.get("BUILD_ID").map_or("(none)", String::as_str),
                os.get("VARIANT_ID").map_or("(none)", String::as_str),
            ),
        );
    }
    let mut steamos = serde_json::Map::new();
    if let Some(s) = read_trim("/etc/steamos-release") {
        steamos.insert("steamos-release".into(), json!(clip(&s, 300)));
    }
    if let Some(m) = std::fs::read_to_string("/etc/steamos-atomupd/manifest.json")
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
    {
        let pick: serde_json::Map<String, Value> = [
            "product", "release", "variant", "arch", "version", "buildid",
        ]
        .iter()
        .filter_map(|k| m.get(*k).map(|v| (k.to_string(), v.clone())))
        .collect();
        steamos.insert("atomupd_manifest".into(), Value::Object(pick));
    }
    o.set("steamos_build", Value::Object(steamos));

    // Kernel (nodename deliberately omitted).
    // SAFETY: zeroed utsname filled by uname(2).
    let uts = unsafe {
        let mut u: libc::utsname = std::mem::zeroed();
        (libc::uname(&mut u) == 0).then_some(u)
    };
    if let Some(u) = uts {
        let f = |b: &[libc::c_char]| {
            let bytes: Vec<u8> = b
                .iter()
                .take_while(|&&c| c != 0)
                .map(|&c| c as u8)
                .collect();
            String::from_utf8_lossy(&bytes).into_owned()
        };
        o.set(
            "kernel",
            json!({
                "sysname": f(&u.sysname),
                "release": f(&u.release),
                "version": f(&u.version),
                "machine": f(&u.machine),
            }),
        );
    }

    // C/C++ runtime baseline.
    let glibc = util::glibc_version();
    let libs: Vec<util::LibProbe> = LIBS
        .iter()
        .map(|l| {
            let mut p = dlopen_probe(l);
            p.path = p.path.map(|x| tilde(&x, home.as_deref()));
            p.error = p.error.map(|e| clip(&tilde(&e, home.as_deref()), 160));
            p
        })
        .collect();
    let lib = |n: &str| libs.iter().find(|l| l.name == n);
    let max_glibc = libc_path().and_then(|p| {
        std::fs::read(p)
            .ok()
            .and_then(|d| parse::max_symbol_version(&d, "GLIBC_2."))
    });
    let max_glibcxx = lib("libstdc++.so.6")
        .and_then(|l| l.path.clone())
        .and_then(|p| std::fs::read(expand(&p, home.as_deref())).ok())
        .and_then(|d| parse::max_symbol_version(&d, "GLIBCXX_3.4."));
    o.set(
        "runtime_baseline",
        json!({ "glibc": glibc, "max_glibc_symbol": max_glibc, "max_glibcxx_symbol": max_glibcxx }),
    );
    o.set("libraries", &libs);
    match &glibc {
        Some(v) => o.finding(
            "glibc",
            Status::Pass,
            &["P7"],
            format!(
                "glibc {v}; system libstdc++ {}",
                max_glibcxx.as_deref().unwrap_or("not found")
            ),
        ),
        None => o.finding("glibc", Status::Unknown, &["P7"], "not a glibc system"),
    }
    match lib("libvulkan.so.1") {
        Some(l) if l.loaded => o.finding(
            "libvulkan",
            Status::Pass,
            &["P9"],
            format!(
                "libvulkan.so.1 found at {}",
                l.path.as_deref().unwrap_or("?")
            ),
        ),
        _ => o.finding(
            "libvulkan",
            Status::Fail,
            &["P9"],
            "libvulkan.so.1 not loadable outside the Steam Runtime",
        ),
    }
    let xr1 = lib("libopenxr_loader.so.1").is_some_and(|l| l.loaded);
    let xr0 = lib("libopenxr_loader.so").is_some_and(|l| l.loaded);
    let (st, msg) = match (xr0, xr1) {
        (true, _) => (Status::Pass, "system OpenXR loader found (libopenxr_loader.so)".to_string()),
        (false, true) => (
            Status::Fail,
            "only libopenxr_loader.so.1 exists; fp-xr's Entry::load() asks for the unversioned libopenxr_loader.so and would fail (load .so.1 or bundle the loader)".to_string(),
        ),
        (false, false) => (
            Status::Fail,
            "no system OpenXR loader; FramePlayer must bundle libopenxr_loader".to_string(),
        ),
    };
    o.finding("openxr_loader", st, &["P7", "I11"], msg);

    // Hardware.
    let cpu = read_trim("/proc/cpuinfo")
        .map(|s| parse::parse_cpuinfo(&s))
        .unwrap_or_default();
    let mem = read_trim("/proc/meminfo")
        .map(|s| parse::parse_meminfo(&s))
        .unwrap_or_default();
    let dt_model = read_trim("/sys/firmware/devicetree/base/model")
        .map(|s| s.trim_end_matches('\0').to_string());
    let dt_compat = std::fs::read("/sys/firmware/devicetree/base/compatible")
        .ok()
        .map(|b| {
            b.split(|&c| c == 0)
                .filter(|s| !s.is_empty())
                .map(|s| String::from_utf8_lossy(s).into_owned())
                .collect::<Vec<_>>()
        });
    let dmi_product = read_trim("/sys/class/dmi/id/product_name");
    o.set(
        "hardware",
        json!({
            "cpu": cpu,
            "available_parallelism": std::thread::available_parallelism().map(|n| n.get()).ok(),
            "mem_total_mb": mem.get("MemTotal").map(|k| k / 1024),
            "mem_available_mb": mem.get("MemAvailable").map(|k| k / 1024),
            "devicetree_model": dt_model,
            "devicetree_compatible": dt_compat,
            "dmi_product": dmi_product,
        }),
    );

    // User and groups.
    // SAFETY: trivial libc getters.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let groups = supplementary_groups();
    let gnames: Vec<String> = groups.iter().map(|&g| util::group_name(g)).collect();
    let has = |n: &str| gnames.iter().any(|g| g == n);
    let login = match (&rc.username, rc.public_username()) {
        (_, Some(u)) => u.to_string(),
        (Some(_), None) => "(custom name, not shown)".to_string(),
        (None, _) => "(unknown)".to_string(),
    };
    o.set(
        "user",
        json!({
            "uid": uid,
            "gid": gid,
            "login": login,
            "groups": gnames,
            "in_video": has("video"),
            "in_render": has("render"),
            "in_audio": has("audio"),
            "in_input": has("input"),
            "is_root": uid == 0,
        }),
    );
    o.finding(
        "login",
        Status::Pass,
        &["I3"],
        format!("login account: {login} (uid {uid})"),
    );

    // Environment and container.
    let vars: Vec<(String, String)> = std::env::vars().collect();
    o.set("env", parse::classify_env(&vars, home.as_deref()));
    let container = json!({
        "pressure_vessel": Path::new("/run/pressure-vessel").exists()
            || vars.iter().any(|(k, _)| k.starts_with("PRESSURE_VESSEL")),
        "run_host": Path::new("/run/host").exists(),
        "flatpak": Path::new("/.flatpak-info").exists(),
        "container_env": std::env::var("container").ok(),
        "launched_over_ssh": std::env::var_os("SSH_CONNECTION").is_some(),
        "launched_by_steam": std::env::var_os("SteamAppId").is_some()
            || std::env::var_os("SteamGameId").is_some(),
    });
    o.set("container", container);

    // OpenXR runtime manifests.
    let manifests = runtime_manifests(home.as_deref());
    let active = manifests.iter().find(|m| m["exists"] == true).cloned();
    o.set("openxr_runtime_manifests", &manifests);
    match active {
        Some(m) => o.finding(
            "active_runtime",
            Status::Pass,
            &["I11"],
            format!(
                "active OpenXR runtime manifest {} -> {}",
                m["path"].as_str().unwrap_or("?"),
                m["name"]
                    .as_str()
                    .or(m["library_path"].as_str())
                    .unwrap_or("?")
            ),
        ),
        None => o.finding(
            "active_runtime",
            Status::Fail,
            &["I11"],
            "no active_runtime.json and no XR_RUNTIME_JSON in this environment",
        ),
    }
    o.set("vulkan_icds", vulkan_icds(home.as_deref()));
    o.set("steamvr", steamvr_info(home.as_deref()));

    let status = if os.is_empty() {
        Status::Unknown
    } else {
        Status::Pass
    };
    let summary = format!(
        "{} {} | kernel {} | glibc {} | libvulkan {} | OpenXR loader {}",
        os.get("NAME").map_or("unknown OS", String::as_str),
        os.get("VERSION_ID").map_or("", String::as_str),
        o.data["kernel"]["release"].as_str().unwrap_or("?"),
        glibc.as_deref().unwrap_or("?"),
        if lib("libvulkan.so.1").is_some_and(|l| l.loaded) {
            "yes"
        } else {
            "no"
        },
        if xr0 || xr1 { "yes" } else { "no" },
    );
    o.finish(status, summary)
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

fn expand(p: &str, home: Option<&str>) -> PathBuf {
    match (p.strip_prefix("~"), home) {
        (Some(rest), Some(h)) => PathBuf::from(format!("{h}{rest}")),
        _ => PathBuf::from(p),
    }
}

fn libc_path() -> Option<String> {
    let maps = std::fs::read_to_string("/proc/self/maps").ok()?;
    parse::mapped_library(&maps, "libc")
}

fn supplementary_groups() -> Vec<u32> {
    // SAFETY: first call sizes, second fills.
    unsafe {
        let n = libc::getgroups(0, std::ptr::null_mut());
        if n <= 0 {
            return Vec::new();
        }
        let mut v = vec![0 as libc::gid_t; n as usize];
        let m = libc::getgroups(n, v.as_mut_ptr());
        v.truncate(m.max(0) as usize);
        v.sort_unstable();
        v.dedup();
        v
    }
}

/// OpenXR loader search order on Linux (XR_RUNTIME_JSON first).
fn runtime_manifests(home: Option<&str>) -> Vec<Value> {
    let mut cands: Vec<PathBuf> = Vec::new();
    if let Some(p) = std::env::var_os("XR_RUNTIME_JSON") {
        cands.push(p.into());
    }
    let arch = std::env::consts::ARCH;
    let names = [
        format!("active_runtime.{arch}.json"),
        "active_runtime.json".to_string(),
    ];
    let mut dirs: Vec<PathBuf> = Vec::new();
    match std::env::var_os("XDG_CONFIG_HOME") {
        Some(x) => dirs.push(PathBuf::from(x)),
        None => dirs.push(util::home().join(".config")),
    }
    let xdg_dirs = std::env::var("XDG_CONFIG_DIRS").unwrap_or_else(|_| "/etc/xdg".into());
    dirs.extend(
        xdg_dirs
            .split(':')
            .filter(|s| !s.is_empty())
            .map(PathBuf::from),
    );
    dirs.push("/etc".into());
    dirs.push("/usr/share".into());
    for d in dirs {
        for n in &names {
            cands.push(d.join("openxr/1").join(n));
        }
    }
    let mut out = Vec::new();
    for p in cands {
        let shown = tilde(&p.display().to_string(), home);
        let Ok(s) = std::fs::read_to_string(&p) else {
            continue;
        };
        let v: Value = serde_json::from_str(&s).unwrap_or(Value::Null);
        let rt = &v["runtime"];
        let lib = rt["library_path"].as_str().map(|l| {
            let base = p.parent().unwrap_or(Path::new("/"));
            let full = if Path::new(l).is_absolute() {
                PathBuf::from(l)
            } else {
                base.join(l)
            };
            (tilde(l, home), full.exists())
        });
        out.push(json!({
            "path": shown,
            "exists": true,
            "name": rt["name"].as_str(),
            "library_path": lib.as_ref().map(|l| l.0.clone()),
            "library_exists": lib.as_ref().map(|l| l.1),
        }));
    }
    out
}

fn vulkan_icds(home: Option<&str>) -> Vec<Value> {
    let mut out = Vec::new();
    for d in ["/usr/share/vulkan/icd.d", "/etc/vulkan/icd.d"] {
        for p in util::list_prefixed(d, "") {
            let v: Value = std::fs::read_to_string(&p)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or(Value::Null);
            out.push(json!({
                "file": tilde(&p.display().to_string(), home),
                "library_path": v["ICD"]["library_path"].as_str(),
                "api_version": v["ICD"]["api_version"].as_str(),
            }));
        }
    }
    out
}

fn steamvr_info(home: Option<&str>) -> Value {
    let h = util::home();
    let roots = [
        h.join(".local/share/Steam"),
        h.join(".steam/steam"),
        h.join(".steam/root"),
    ];
    for r in roots {
        let acf = r.join("steamapps/appmanifest_250820.acf");
        if let Ok(s) = std::fs::read_to_string(&acf) {
            let dir = r.join("steamapps/common/SteamVR");
            return json!({
                "steam_root": tilde(&r.display().to_string(), home),
                "steamvr_buildid": parse::acf_value(&s, "buildid"),
                "steamvr_installed": dir.exists(),
                "steamvr_beta": parse::acf_value(&s, "BetaKey"),
            });
        }
    }
    json!({ "steamvr_appmanifest": "not found" })
}
